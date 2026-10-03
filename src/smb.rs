//! Direct SMB2/3 access from the headset: no mount, no helper process.
//!
//! Playback reads go through [`SmbReader`], a [`ReadAheadReader`] over the file.

use crate::readahead::{BlockSource, BoxFuture, ReadAhead, ReadAheadReader};
use anyhow::{Context, bail, ensure};
use futures_util::StreamExt;
use smb::{
    Client, ClientConfig, ConnectionConfig, DirAccessMask, Directory, File, FileAccessMask,
    FileCreateArgs, FileDirectoryInformation, UncPath,
};
use std::{
    io,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::runtime::Runtime;

/// `smb://[domain;]user@host[:port][/share[/path]]`. Passwords are never part of
/// the URL. Without a share the URL names the server (browse its shares).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmbUrl {
    pub domain: Option<String>,
    pub user: String,
    pub host: String,
    pub port: Option<u16>,
    pub share: String,
    pub path: String,
}

impl FromStr for SmbUrl {
    type Err = anyhow::Error;

    fn from_str(url: &str) -> anyhow::Result<Self> {
        let rest = url
            .strip_prefix("smb://")
            .context("SMB URLs start with smb://")?;
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        if let Some(userinfo) = userinfo {
            ensure!(
                !userinfo.contains(':'),
                "Do not put passwords in SMB URLs; use JUST_VIDEO_SMB_PASSWORD or the prompt"
            );
        }
        let (domain, user) = match userinfo.map(|u| u.split_once(';').unwrap_or(("", u))) {
            Some(("", user)) => (None, user.to_string()),
            Some((domain, user)) => (Some(domain.to_string()), user.to_string()),
            None => (None, "guest".to_string()),
        };
        // `host`, `host:port`, `[v6]` or `[v6]:port`.
        let port_split = match hostport.rfind(']') {
            Some(close) => hostport[close..].rfind(':').map(|i| close + i),
            None => hostport.rfind(':'),
        };
        let (host, port) = match port_split {
            Some(i) => (
                hostport[..i].to_string(),
                Some(hostport[i + 1..].parse().context("Invalid SMB port")?),
            ),
            None => (hostport.to_string(), None),
        };
        ensure!(!host.is_empty(), "SMB URL needs a host");
        let mut parts = path.split('/').filter(|p| !p.is_empty());
        let share = parts.next().unwrap_or_default().to_string();
        let segments: Vec<&str> = parts.collect();
        if segments
            .iter()
            .any(|s| *s == ".." || *s == "." || s.contains('\\'))
        {
            bail!("SMB paths may not contain '.', '..' or backslashes");
        }
        Ok(Self {
            domain,
            user,
            host,
            port,
            share,
            path: segments.join("\\"),
        })
    }
}

impl SmbUrl {
    fn server(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    fn unc(&self, share: &str, path: &str) -> anyhow::Result<UncPath> {
        ensure!(!share.is_empty(), "No share selected");
        let unc = UncPath::new(&self.server())?.with_share(share)?;
        Ok(if path.is_empty() {
            unc
        } else {
            unc.with_path(path)
        })
    }

    /// The server alone (`smb://user@host[:port]`), e.g. for saving.
    pub fn server_url(&self) -> String {
        let user = match &self.domain {
            Some(domain) => format!("{domain};{}", self.user),
            None => self.user.clone(),
        };
        format!("smb://{user}@{}", self.server())
    }

    fn login_name(&self) -> String {
        match &self.domain {
            Some(domain) => format!("{domain}\\{}", self.user),
            None => self.user.clone(),
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// One authenticated server connection plus the runtime that drives it.
/// Shares are connected on first use with the same credentials.
pub struct SmbSession {
    runtime: Arc<Runtime>,
    // Always Some until drop; taken so it is released inside the runtime.
    client: Option<Arc<Client>>,
    url: SmbUrl,
    password: String,
    connected: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// Longest a browsing request may take. A request that never completes means
/// the connection is wedged (smb-rs can run out of credits); the caller then
/// drops this session and reconnects.
const REQUEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

fn run_with_deadline<T>(
    runtime: &Runtime,
    what: &str,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    runtime
        .block_on(async { tokio::time::timeout(REQUEST_DEADLINE, future).await })
        .map_err(|_| anyhow::anyhow!("The server did not answer ({what})"))?
}

impl SmbSession {
    /// Authenticates with the server: against the URL's share if it names one,
    /// otherwise against IPC$ (which also enables share listing).
    pub fn connect(url: SmbUrl, password: String) -> anyhow::Result<Self> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("smb-io")
                .enable_all()
                .build()?,
        );
        // smb-rs spawns cleanup tasks when connections, trees and handles drop,
        // including on error paths, so they must be dropped inside the runtime.
        let _guard = runtime.enter();
        let client = Arc::new(Client::new(ClientConfig {
            connection: ConnectionConfig {
                // Every server we target speaks SMB2+; skipping SMB1 saves a round trip.
                smb2_only_negotiate: true,
                // Video is incompressible; compression would only burn headset CPU.
                compression_enabled: false,
                // A 1 MiB read costs 16 credits; allow a full read-ahead window in flight.
                credits_backlog: Some(1024),
                ..Default::default()
            },
            ..Default::default()
        }));
        let session = Self {
            runtime: runtime.clone(),
            client: Some(client),
            url,
            password,
            connected: Default::default(),
        };
        if session.url.share.is_empty() {
            run_with_deadline(&runtime, "sign in", async {
                Ok(session
                    .client()
                    .ipc_connect(
                        &session.url.server(),
                        &session.url.login_name(),
                        session.password.clone(),
                    )
                    .await?)
            })
            .with_context(|| format!("Sign in to {}", session.url.host))?;
        } else {
            session.ensure_share(&session.url.share.clone())?;
        }
        drop(_guard);
        Ok(session)
    }

    fn client(&self) -> &Client {
        self.client.as_ref().expect("client lives until drop")
    }

    pub fn url(&self) -> &SmbUrl {
        &self.url
    }

    fn ensure_share(&self, share: &str) -> anyhow::Result<()> {
        let mut connected = self.connected.lock().expect("share set");
        if connected.contains(share) {
            return Ok(());
        }
        let unc = self.url.unc(share, "")?;
        run_with_deadline(&self.runtime, "open share", async {
            Ok(self
                .client()
                .share_connect(&unc, &self.url.login_name(), self.password.clone())
                .await?)
        })
        .with_context(|| format!("Open share \\\\{}\\{share}", self.url.host))?;
        connected.insert(share.to_string());
        Ok(())
    }

    /// Disk shares on the server, without administrative ones (`C$`, `IPC$`).
    pub fn shares(&self) -> anyhow::Result<Vec<String>> {
        let server = self.url.server();
        let mut names = run_with_deadline(&self.runtime, "list shares", async {
            if !self.connected.lock().expect("share set").contains("IPC$") {
                self.client()
                    .ipc_connect(&server, &self.url.login_name(), self.password.clone())
                    .await?;
            }
            let pipe = self.client().open_pipe(&server, "srvsvc").await?;
            let transceive = |request: Vec<u8>| {
                let pipe = &pipe;
                async move {
                    let reply = pipe
                        .fsctl_with_options(
                            smb::PipeTransceiveRequest::from(smb::IoctlBuffer::from(request)),
                            65536,
                        )
                        .await?;
                    anyhow::Ok::<Vec<u8>>(reply.0.to_vec())
                }
            };
            crate::srvsvc::check_bind_ack(&transceive(crate::srvsvc::bind_request()).await?)?;
            let reply = transceive(crate::srvsvc::share_enum_request(&self.url.host)).await?;
            let shares = crate::srvsvc::parse_share_enum_response(&reply)?;
            let _ = pipe.close().await;
            anyhow::Ok(
                shares
                    .into_iter()
                    .filter(|s| s.is_browsable_disk())
                    .map(|s| s.name)
                    .collect::<Vec<_>>(),
            )
        })?;
        self.connected
            .lock()
            .expect("share set")
            .insert("IPC$".into());
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    /// Lists the directory named by the session URL (or `path` relative to its share).
    pub fn list(&self, path: &str) -> anyhow::Result<Vec<Entry>> {
        self.list_in(&self.url.share.clone(), path)
    }

    /// Lists `path` (backslash separated, empty for the root) inside `share`.
    pub fn list_in(&self, share: &str, path: &str) -> anyhow::Result<Vec<Entry>> {
        self.ensure_share(share)?;
        let unc = self.url.unc(share, path)?;
        run_with_deadline(&self.runtime, "list folder", async {
            let access = DirAccessMask::new()
                .with_list_directory(true)
                .with_synchronize(true);
            let resource = self
                .client()
                .create_file(&unc, &FileCreateArgs::make_open_existing(access.into()))
                .await?;
            let directory = Arc::new(resource.unwrap_dir());
            let mut entries = Vec::new();
            {
                let mut stream =
                    Directory::query::<FileDirectoryInformation>(&directory, "*").await?;
                while let Some(item) = stream.next().await {
                    let item = item?;
                    let name = item.file_name.to_string();
                    if name == "." || name == ".." {
                        continue;
                    }
                    entries.push(Entry {
                        name,
                        is_dir: item.file_attributes.directory(),
                        size: item.end_of_file,
                    });
                }
            }
            directory.close().await?;
            entries.sort_by(|a, b| {
                (!a.is_dir, a.name.to_lowercase()).cmp(&(!b.is_dir, b.name.to_lowercase()))
            });
            anyhow::Ok(entries)
        })
    }

    pub fn open(&self, path: &str, options: ReadAhead) -> anyhow::Result<SmbReader> {
        self.open_in(&self.url.share.clone(), path, options)
    }

    pub fn open_in(
        &self,
        share: &str,
        path: &str,
        options: ReadAhead,
    ) -> anyhow::Result<SmbReader> {
        let (file, len) = self.open_file(share, path)?;
        Ok(ReadAheadReader::new(
            self.runtime.clone(),
            SmbFile::single(file, None),
            len,
            options,
        ))
    }

    /// Like [`SmbSession::open_in`], but the reader keeps this session alive:
    /// give each playing video its own session, so a wedged connection only
    /// affects that video and closes with it.
    pub fn open_owned(
        self: &Arc<Self>,
        share: &str,
        path: &str,
        options: ReadAhead,
    ) -> anyhow::Result<SmbReader> {
        Self::open_striped(std::slice::from_ref(self), share, path, options)
    }

    /// Like [`SmbSession::open_owned`] over several sessions (connections) to
    /// the same server: each read goes to the least busy one. Over Wi-Fi two
    /// TCP connections carry 30-50 % more than one, and ride out a stall of
    /// one of them. Sessions after the first that can't open the file are
    /// left out.
    pub fn open_striped(
        sessions: &[Arc<Self>],
        share: &str,
        path: &str,
        options: ReadAhead,
    ) -> anyhow::Result<SmbReader> {
        let first = sessions.first().context("No connection")?;
        let (file, len) = first.open_file(share, path)?;
        let mut source = SmbFile::single(file, Some(first.clone()));
        for session in &sessions[1..] {
            match session.open_file(share, path) {
                Ok((file, _)) => source.lanes.push(Lane {
                    file,
                    busy: Default::default(),
                    _session: Some(session.clone()),
                }),
                Err(e) => eprintln!("SMB: reading {path} over one connection less: {e:#}"),
            }
        }
        Ok(ReadAheadReader::new(
            first.runtime.clone(),
            source,
            len,
            options,
        ))
    }

    /// Renames `path` (backslash separated, inside `share`) to `new_name` in
    /// the same folder.
    pub fn rename_in(&self, share: &str, path: &str, new_name: &str) -> anyhow::Result<()> {
        ensure!(
            !new_name.is_empty()
                && !new_name.contains(['\\', '/'])
                && new_name != "."
                && new_name != "..",
            "Names can't be empty or contain slashes"
        );
        let target = match path.rsplit_once('\\') {
            Some((parent, _)) => format!("{parent}\\{new_name}"),
            None => new_name.to_string(),
        };
        self.modify(share, path, "rename", |handle| {
            Box::pin(async move {
                handle
                    .set_info(smb::FileRenameInformation {
                        replace_if_exists: false.into(),
                        root_directory: 0,
                        file_name: target.into(),
                    })
                    .await
            })
        })
    }

    /// Deletes a file or an empty folder.
    pub fn delete_in(&self, share: &str, path: &str) -> anyhow::Result<()> {
        self.modify(share, path, "delete", |handle| {
            Box::pin(async move {
                handle
                    .set_info(smb::FileDispositionInformation::default())
                    .await
            })
        })
    }

    fn modify(
        &self,
        share: &str,
        path: &str,
        what: &str,
        change: impl FnOnce(&smb::ResourceHandle) -> BoxFuture<'_, smb::Result<()>>,
    ) -> anyhow::Result<()> {
        self.ensure_share(share)?;
        let unc = self.url.unc(share, path)?;
        run_with_deadline(&self.runtime, what, async {
            let access = FileAccessMask::new()
                .with_delete(true)
                .with_synchronize(true);
            let resource = self
                .client()
                .create_file(&unc, &FileCreateArgs::make_open_existing(access))
                .await
                .map_err(|e| explain_write_error(&e.to_string()))?;
            let handle = match (resource.as_file(), resource.as_dir()) {
                (Some(f), _) => f.handle(),
                (_, Some(d)) => d.handle(),
                _ => bail!("Not a file or folder"),
            };
            let result = change(handle).await;
            let _ = handle.close().await;
            result.map_err(|e| explain_write_error(&e.to_string()))
        })
    }

    fn open_file(&self, share: &str, path: &str) -> anyhow::Result<(File, u64)> {
        self.ensure_share(share)?;
        let unc = self.url.unc(share, path)?;
        let file = run_with_deadline(&self.runtime, "open file", async {
            let args =
                FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
            let resource = self.client().create_file(&unc, &args).await?;
            ensure!(resource.is_file(), "{path} is not a file");
            anyhow::Ok(resource.unwrap_file())
        })?;
        let len = self.runtime.block_on(smb::GetLen::get_len(&file))?;
        Ok((file, len))
    }
}

impl Drop for SmbSession {
    fn drop(&mut self) {
        let _guard = self.runtime.enter();
        if let Some(client) = self.client.take() {
            // A wedged connection may never answer the logoff.
            let _ = self.runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), client.close()).await
            });
            drop(client);
        }
    }
}

/// Turns SMB status codes from changing files into plain explanations.
fn explain_write_error(message: &str) -> anyhow::Error {
    let m = message.to_ascii_lowercase();
    let text = if m.contains("0xc0000022") {
        "The server doesn't allow changing this (you have no write permission here)."
    } else if m.contains("0xc0000035") {
        "Something with that name already exists."
    } else if m.contains("0xc0000101") {
        "The folder isn't empty. Only empty folders can be deleted."
    } else if m.contains("0xc0000043") {
        "The file is in use (maybe playing somewhere). Try again later."
    } else if m.contains("0xc0000034") || m.contains("0xc000003a") {
        "It's no longer there (renamed or deleted elsewhere)."
    } else {
        return anyhow::anyhow!("{message}");
    };
    anyhow::anyhow!("{text}")
}

/// An open SMB file as a read-ahead block source.
/// An open file, read over one or more connections ("lanes").
pub struct SmbFile {
    lanes: Vec<Lane>,
}

/// The file opened on one connection.
struct Lane {
    file: File,
    /// Bytes requested on this lane and not yet received.
    busy: AtomicUsize,
    /// Keeps a dedicated session (and its connection) alive while reading.
    _session: Option<Arc<SmbSession>>,
}

impl SmbFile {
    fn single(file: File, session: Option<Arc<SmbSession>>) -> Self {
        Self {
            lanes: vec![Lane {
                file,
                busy: Default::default(),
                _session: session,
            }],
        }
    }

    /// The lane with the least data still to come: a read after a jump goes
    /// past reads left from before it (they can't be cancelled).
    fn least_busy(&self) -> usize {
        (0..self.lanes.len())
            .min_by_key(|&i| self.lanes[i].busy.load(Ordering::SeqCst))
            .unwrap_or(0)
    }
}

/// Gives a lane's bytes back when its read ends (or is dropped).
struct Busy<'a>(&'a AtomicUsize, usize);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(self.1, Ordering::SeqCst);
    }
}

impl BlockSource for SmbFile {
    fn fetch(self: Arc<Self>, offset: u64, len: usize) -> BoxFuture<'static, io::Result<Vec<u8>>> {
        Box::pin(async move {
            let lane = &self.lanes[self.least_busy()];
            lane.busy.fetch_add(len, Ordering::SeqCst);
            let _busy = Busy(&lane.busy, len);
            let mut buffer = vec![0u8; len];
            let mut filled = 0;
            while filled < len {
                let n = lane
                    .file
                    .read_block(&mut buffer[filled..], offset + filled as u64, None, false)
                    .await?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "SMB file shorter than reported",
                    ));
                }
                filled += n;
            }
            Ok(buffer)
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        // Closing marks the handle closed, so late drops of aborted reads are no-ops.
        Box::pin(async {
            for lane in &self.lanes {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), lane.file.close())
                    .await;
            }
        })
    }
}

pub type SmbReader = ReadAheadReader<SmbFile>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_url() {
        let url: SmbUrl = "smb://WORK;alice@nas.local:4445/Media/VR/clip.mp4"
            .parse()
            .unwrap();
        assert_eq!(url.domain.as_deref(), Some("WORK"));
        assert_eq!(url.user, "alice");
        assert_eq!(url.host, "nas.local");
        assert_eq!(url.port, Some(4445));
        assert_eq!(url.share, "Media");
        assert_eq!(url.path, "VR\\clip.mp4");
        assert_eq!(url.login_name(), "WORK\\alice");
    }

    #[test]
    fn share_only_and_guest() {
        let url: SmbUrl = "smb://[fe80::1]:445/videos/".parse().unwrap();
        assert_eq!(url.host, "[fe80::1]");
        let url: SmbUrl = "smb://192.168.1.5/videos/".parse().unwrap();
        assert_eq!(url.user, "guest");
        assert_eq!(url.port, None);
        assert_eq!(url.share, "videos");
        assert_eq!(url.path, "");
    }

    #[test]
    fn server_only_url() {
        let url: SmbUrl = "smb://alice@192.168.1.10".parse().unwrap();
        assert_eq!(
            (url.host.as_str(), url.share.as_str()),
            ("192.168.1.10", "")
        );
        assert_eq!(url.server_url(), "smb://alice@192.168.1.10");
        let url: SmbUrl = "smb://W;bob@nas:4445/".parse().unwrap();
        assert_eq!(url.server_url(), "smb://W;bob@nas:4445");
    }

    #[test]
    fn rejects_passwords_and_traversal() {
        assert!("smb://bob:secret@host/share".parse::<SmbUrl>().is_err());
        assert!("smb://host/share/../other".parse::<SmbUrl>().is_err());
        assert!("http://host/share".parse::<SmbUrl>().is_err());
    }
}
