# Changelog

## Unreleased

- Surround sound (5.1 and 7.1, such as the DTS and AC3 tracks of 3D Blu-ray
  copies) no longer crackles in loud scenes: mixed down to stereo it could add
  up to more than full scale, and a limiter now keeps it within.
- 3D films tagged `HOU`, `FOU`, `HalfSBS`, `FullSBS`, `HalfOU` or `FullOU`
  are recognized as 3D.

## v0.1.0 (2026-10-06)

First release of this fork, built from upstream `kumorig/just-video` (main of
2026-09-27) with its open pull requests merged:

- Videos on the headset itself: "This headset" (Videos, Downloads, home,
  SD cards, USB drives) next to SMB servers (#2).
- Thumbnails in video lists, Settings screen, D-pad navigation and volume,
  faster seeking and SMB reads, two frames in flight in the renderer, audio
  sync, a sturdier hardware decoder, Steam library art (#5–#15).
- Half-SBS/half-OU films shown in their real shape (#4).

New in this fork:

- All videos of a folder and its subfolders in one list (folders button).
- Windows build with Docker Desktop (`windows/build.ps1`).
- Two ways to install: on the headset from the release zip
  (`install-on-frame.sh`, no PC or Developer Mode needed), or from a PC over
  SSH with Developer Mode (`deploy.ps1` for Windows and `deploy.sh` for Linux
  and macOS, both in the zip). The release zip carries the license files.
- README in English and German.
