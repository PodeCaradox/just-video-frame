# Just Video: development notes

A Rust VR video player for the Steam Frame (aarch64 SteamOS headset) that plays
from SMB shares: OpenXR + Vulkan rendering, FFmpeg decoding (V4L2 hardware
decoder `iris` on the headset), a built-in SMB client in `third_party/`.

## Build and test

- PC checks: `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`.
  Needs FFmpeg dev headers (`.cargo/config.toml` points `PKG_CONFIG_PATH` at
  `.local-deps/sysroot`). `readahead::tests::pipelining_hides_latency` is
  timing-flaky under load: rerun it alone before suspecting a real failure.
- UI tests and `ui-preview` need a font: on Debian/Ubuntu run
  `sudo apt install fonts-noto-core` (Noto Sans).
- Headset build: `bash scripts/build-frame-media.sh` once (FFmpeg + dav1d, with
  the patches in `third_party/ffmpeg-patches/`), then `bash scripts/build-frame.sh`
  (cargo-zigbuild, aarch64, static FFmpeg).
- After changing a patch, rebuild the media libs; the FFmpeg source is
  patched only when first unpacked.

## Deploy and SSH

- `bash scripts/install-frame.sh` copies the binary, launcher and Steam art to
  `~/Applications/JustVideo` on the headset. `FRAME_RESTART_STEAM=1` restarts
  Steam once (needed to set the VR-app flag and icon). It keeps no old
  binary: to roll back, rebuild an older commit.
- `bash scripts/frame-ssh.sh '<command>'` runs a command on the headset with the
  dev key. The address defaults to `frame.local`; set `FRAME_HOST=<ip>` if
  that doesn't resolve.
- The app logs to `~/Applications/JustVideo/just-video.log` (previous run in
  `.log.1`) when started from Steam.
- Don't leave extra binaries (`.prev`, `.test`) on the headset.

## Testing on the headset

- Steam suspends the headset after an hour without input, even on the charger.
  `bash scripts/frame-keep-awake.sh [HOURS]` holds a logind inhibitor (`stop`,
  `status` also work). Caveat: Steam then stays in its sleep power state until
  someone uses the headset. VR apps don't draw, and Steam's web helper keeps a
  second decoder session open (see below), so 8K can't use the hardware
  decoder. 6K and 1080p tests over SSH are unaffected.
- Mute before any playback test:
  `bash scripts/frame-ssh.sh 'wpctl set-mute @DEFAULT_AUDIO_SINK@ 1'`.
- Nobody wears the headset during SSH tests, and SteamVR then doesn't ask for
  frames. `JUST_VIDEO_RENDER_UNSEEN=1` makes the frame loop draw anyway
  (real swapchains, upload, shader, `xrEndFrame`). Launch through Steam to get the
  app's SteamVR settings.
- Reboot without sudo:
  `systemd-run --user --quiet --collect --unit=just-video-reboot systemctl reboot --no-ask-password`
  (run it through `frame-ssh.sh`). Re-run keep-awake after the headset is back.
- `just-video bench-seek <smb-url>` measures open and jump times; it exits
  non-zero when a jump never shows a picture, playback stops, or decoding moves
  to the CPU. `--play N --jump-every S --jump-by D --hz 60` runs steady
  playback with jumps (negative D jumps back). The app also logs
  `Timing: frames ...` every 5 s while playing.

## Hardware decoder (iris) session load

The `iris` driver adds up the load of every open V4L2 decoder session when one
starts streaming, and counts each as the starting session (its macroblocks per
frame and frame rate, at least 30 fps). Limits: 278528 macroblocks per frame
and 7833600 macroblocks per second. Steam's own web helper is a session too.
So with one other session a new 6K/8K decoder passes, but a flushed one that
played 60 fps content is refused (`qcom-iris: current session not
supported(-12)` in the kernel log); with two other sessions 8K never opens.
That is why the player may replace the decoder at a jump instead of flushing
it, and why a refusal alone isn't a player bug.

## Benchmarking

- Wi-Fi noise swamps totals. Run alternating pairs (old build, new build,
  old, new, ...) and compare medians, not single runs or means.
- Split timings into phases (network wait, decode, copy, render) before
  deciding what to optimize.
- Check the first run after a reboot or a Steam restart separately: the
  decoder state differs (Steam's helper session).

## Commits

- Never put private file or folder names, server names, user names, Wi-Fi names,
  passwords or local paths into code, tests, docs or commit messages. Use
  generic names (`a_180_sbs.mp4`, `smb://alice@192.168.1.10`). Grep the staged
  diff before committing.
