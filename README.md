```
AI Disclaimer: Text below is AI generated, sorry I'm lazy.
Most of the code is as well.
AI contributions are welcome (as to not be a hypocrite).  
```

# Just Video

A VR video player for Steam Frame that streams straight from SMB shares. (You make butter.)

## Build

On a Linux PC with Rust, `curl`, `make`, `pkg-config` and `python3`:

```sh
rustup target add aarch64-unknown-linux-gnu
cargo install cargo-zigbuild
bash scripts/build-frame-media.sh   # once: FFmpeg + dav1d for the Frame
bash scripts/build-frame.sh
```

## Install on Steam Frame

1. On the Frame, enable **Developer Mode** and set a user password under **Developer**.
2. From the PC, set up SSH once:
   ```sh
   ssh-keygen -t ed25519 -f ~/.ssh/steam_frame_ed25519
   ssh-copy-id -i ~/.ssh/steam_frame_ed25519 steamos@frame.local
   ```
3. Install (adds **Just Video** to the Steam library):
   ```sh
   bash scripts/install-frame.sh   # FRAME_HOST=<ip> if frame.local doesn't resolve
   ```

## Run

Start **Just Video** from the Steam library on the Frame, choose **Add server**
and enter your SMB server's address and login.

Videos stored on the Frame itself are under **This headset**: Videos, Downloads,
the home folder, and any SD card or USB drive.
