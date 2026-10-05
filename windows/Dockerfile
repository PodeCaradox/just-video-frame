# Build environment for the Steam Frame binary (aarch64 SteamOS), used by
# windows/build.ps1 on a Windows PC with Docker Desktop. The build itself
# (FFmpeg + dav1d cross-compiled with zig, then cargo-zigbuild) is the same
# as on a Linux PC: scripts/build-frame-media.sh and scripts/build-frame.sh.
FROM rust:1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      binutils-aarch64-linux-gnu ca-certificates curl git make ninja-build \
      patch pkg-config python3 python3-venv rsync xz-utils zstd \
 && rm -rf /var/lib/apt/lists/*

# FFmpeg's configure inspects the cross-compiled objects with `nm`.
RUN mkdir -p /opt/shim && ln -s /usr/bin/aarch64-linux-gnu-nm /opt/shim/nm

RUN rustup target add aarch64-unknown-linux-gnu \
 && cargo install --locked cargo-zigbuild \
 && rm -rf /usr/local/cargo/registry

WORKDIR /build
