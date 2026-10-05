#!/usr/bin/env bash
# Runs inside the build container (started by windows/build.ps1).
# /src is the repository (read only), /out its out\ folder. The media stack,
# Cargo's downloads and the target folder live in Docker volumes, so only the
# first build compiles FFmpeg and dav1d.
# Writes out/build.log and out/STATUS (STARTED, then OK or FAILED <step>),
# and the package for the headset to out/JustVideo.
set -uo pipefail
out=/out
mkdir -p "$out"

fail() {
    echo "FAILED: $1"
    echo "FAILED $1 $(date -u +%FT%TZ)" > "$out/STATUS"
    exit 1
}

main() {
    echo "STARTED $(date -u +%FT%TZ)" > "$out/STATUS"
    local src=/build/src
    mkdir -p "$src"
    # Uncommitted changes count too (the build ID marks them with "+").
    rsync -a --delete --exclude /target --exclude /.local-deps --exclude /out /src/ "$src/" ||
        fail copy
    cd "$src" || fail copy
    git config --global --add safe.directory '*'
    echo "== Source $(git log -1 --format='%h %s' 2>/dev/null)"

    if [ ! -f .local-deps/frame-media/lib/libavcodec.a ]; then
        echo "== FFmpeg + dav1d for the Frame (first build only, takes a while)"
        PATH=/opt/shim:$PATH bash scripts/build-frame-media.sh || fail media
    fi

    # The crates as an archive, for checking changes on a machine without
    # internet access to crates.io. Not needed for the build itself.
    if [ ! -f "$out/vendor.tar.zst" ] || [ Cargo.lock -nt "$out/vendor.tar.zst" ]; then
        echo "== Vendoring crates"
        rm -rf /tmp/vendor
        if cargo vendor --locked /tmp/vendor > /dev/null &&
            tar -C /tmp -cf - vendor | zstd -q -T0 -10 -o "$out/vendor.tar.zst.tmp"; then
            mv -f "$out/vendor.tar.zst.tmp" "$out/vendor.tar.zst"
        else
            echo "warning: vendoring failed (the build doesn't need it)"
        fi
    fi

    echo "== Just Video for the Frame (aarch64)"
    bash scripts/build-frame.sh || fail cargo

    echo "== Package"
    local pkg=$out/JustVideo
    rm -rf "$pkg"
    mkdir -p "$pkg"
    cp target/aarch64-unknown-linux-gnu/release/just-video "$pkg/" || fail package
    cp scripts/steam-shortcut.py frame/install-on-frame.sh "$pkg/" || fail package
    cp -r assets/steam "$pkg/art" || fail package
    cp LICENSE THIRD_PARTY_NOTICES.md frame/INSTALL.txt "$pkg/" || fail package
    cp -r licenses "$pkg/licenses" || fail package
    git log -1 --format='%h %cs' > "$pkg/VERSION" 2>/dev/null || true

    # The same folder as a zip for a GitHub release (file modes kept).
    local version zip
    version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
    zip=$out/just-video-frame-v$version-steamframe-arm64.zip
    rm -f "$out"/just-video-frame-*-steamframe-arm64.zip
    python3 windows/make-zip.py "$pkg" "$zip" || fail zip
    echo "OK $(date -u +%FT%TZ) $(cat "$pkg/VERSION" 2>/dev/null)" > "$out/STATUS"
    echo "== Done: out/JustVideo and $(basename "$zip")"
}

main 2>&1 | tee "$out/build.log"
exit "${PIPESTATUS[0]}"
