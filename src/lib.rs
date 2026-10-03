//! Just Video: standalone Steam Frame VR player reading directly from SMB shares.

#[cfg(feature = "decode")]
pub mod audio;
#[cfg(feature = "decode")]
pub mod bench;
pub mod config;
#[cfg(feature = "decode")]
pub mod decode;
pub mod inventory;
#[cfg(feature = "decode")]
pub mod library;
#[cfg(feature = "decode")]
pub mod media;
#[cfg(feature = "decode")]
pub mod playability;
pub mod readahead;
pub mod smb;
pub mod srvsvc;
pub mod subtitles;
pub mod system_volume;
#[cfg(feature = "decode")]
pub mod ui;
#[cfg(feature = "decode")]
pub mod vr;
#[cfg(feature = "decode")]
pub mod xr;
