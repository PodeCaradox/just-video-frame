# Just Video for Steam Frame

`#SteamFrame` `#VR` `#VR180` `#VR360` `#VideoPlayer` `#OpenXR` `#Vulkan` `#Rust` `#FFmpeg` `#AV1` `#SteamOS`

A VR video player that runs **standalone on Valve's Steam Frame**: no PC
needed to play. It reads videos from the headset's own storage or straight
from a Windows/NAS share on your network, without copying them first.

This is a community fork of [kumorig/just-video](https://github.com/kumorig/just-video)
(MIT). It merges the upstream project with its open pull requests and adds a
Windows build, a headset installer and a list of all videos in subfolders.
Deutsche Anleitung: [LIESMICH.md](LIESMICH.md).

> Unofficial and experimental. Not affiliated with Valve.

## Features

- **Hardware decoding** of H.264, H.265/HEVC and VP9 (8-bit) through the Frame's
  own video decoder, up to 8K (8192×8192).
- **AV1** with dav1d on the CPU: 4K at 60 fps decodes faster than real time.
  8K AV1 and 8K 10-bit HEVC are too heavy for the CPU (the hardware decoder
  only does 8-bit).
- **VR formats:** flat, VR180, VR360 and fisheye; side by side, top/bottom,
  swapped eyes. Detected from the file's metadata or name tags (`_180`, `_360`,
  `_LR`, `_TB`, `_3dh`, `180x180`, `fisheye190` …) and from 2:1 frames of 5.7K
  and wider; can be changed per video and is remembered.
- **Sources:**
  - *This headset*: Videos, Downloads, the home folder, SD cards and USB drives.
  - *SMB shares* (Windows PC, NAS, Samba): played directly over the network.
- **Browser:** thumbnails (header button), all videos of a folder and its
  subfolders in one list (folders button), playability check before opening.
- **Playback:** resume where you left off, next/previous, subtitles (`.srt`
  next to the video and embedded), audio tracks, picture adjustments.

Supported containers: `mp4`, `m4v`, `mkv`, `mov`, `webm`, `avi`, `ts`, `m2ts`.

## Controls

| Action | Button |
| --- | --- |
| Point / select | Aim the controller, trigger or **A** |
| Back | **B** |
| Scroll lists | Thumbstick up/down (hold grip: faster) |
| Jump back / forward | D-pad left/right or flick the stick sideways (hold grip: longer jump) |
| Volume | D-pad up/down |
| Recenter the view | Press the thumbstick |

Jump lengths, volume step and resuming are on the **Settings** screen.

## Build

### Windows with Docker Desktop

Start Docker Desktop, then in PowerShell in the repository folder:

```powershell
powershell -ExecutionPolicy Bypass -File windows\build.ps1
```

The first build compiles FFmpeg and dav1d for the Frame once (kept in Docker
volumes); later builds take a few minutes. Output: `out\JustVideo`, log:
`out\build.log`.

### Linux

With Rust, `curl`, `make`, `ninja`, `pkg-config` and `python3`:

```sh
rustup target add aarch64-unknown-linux-gnu
cargo install cargo-zigbuild
bash scripts/build-frame-media.sh   # once: FFmpeg + dav1d for the Frame
bash scripts/build-frame.sh
```

## Install on the Steam Frame

1. On the Frame: **Settings → System → Developer Mode** on, then set a
   **user password** under **Developer**.
2. From Windows:

   ```powershell
   powershell -ExecutionPolicy Bypass -File windows\deploy.ps1
   # or, if frame.local isn't found:
   powershell -ExecutionPolicy Bypass -File windows\deploy.ps1 -Frame <frame-ip>
   ```

   From Linux: set up an SSH key for `steamos@frame.local` and run
   `bash scripts/install-frame.sh`.
3. On the Frame: **Library → Non-Steam → Just Video**.

The first install restarts Steam on the headset once: the entry must be marked
as a VR app, and Steam only reads that flag at startup. You can also copy
`out/JustVideo` to the headset yourself and run `bash install-on-frame.sh` in
Konsole there.

## Stream videos from a PC

1. Share the video folder on the PC: right-click → **Properties → Sharing →
   Advanced Sharing → Share this folder** (read access is enough). File sharing
   must be allowed for the network (private network profile).
2. In Just Video choose **Add server** and enter the PC's address and your
   Windows user name and password.

Use the 6 GHz link of the Frame's PC adapter if you can: it is much faster than
a 2.4 GHz home network, which can't keep up with 8K files.

## Troubleshooting

- The app's log: `~/Applications/JustVideo/just-video.log` on the headset.
- An 8K video won't open or falls back to the CPU: restart the headset. The
  hardware decoder's capacity is shared with other open decoder sessions
  (Steam's own web helper holds one).
- A video plays flat or doubled: change its format in the player; the choice is
  saved for that file.

## Credits and license

MIT, see [LICENSE](LICENSE). Third-party licences (FFmpeg LGPL-2.1, dav1d,
zlib): [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

- [Just Video](https://github.com/kumorig/just-video) by kumorig: player, SMB
  client, renderer.
- Open pull requests merged here: Nick Vance (#5–#15: thumbnails, settings,
  D-pad, faster seeking and SMB reads, renderer pipelining, Steam library art),
  Leonhard Gruenschloss (#2: videos on the headset, #4: half-SBS/OU films).
- This fork: Windows/Docker build, headset installer, videos in subfolders.

Upstream notes: most of the code was written with AI assistance, as the original
project says; [CLAUDE.md](CLAUDE.md) and [docs/](docs) describe its development
and measurements on the headset.
