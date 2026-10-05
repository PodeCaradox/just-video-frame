#!/usr/bin/env bash
# Installs Just Video on the Steam Frame from a package folder (out/JustVideo,
# made by windows/build.ps1): run on the headset, from anywhere, e.g. over SSH
# by windows/deploy.ps1, or in Konsole after copying the folder over by hand.
#
# Copies the app to ~/Applications/JustVideo and adds "Just Video" to the
# Steam library (Non-Steam). The entry must be marked as a VR app, or SteamVR
# keeps its own menu in front and the app starts "in the background". Steam
# reads that flag only at startup and rewrites the file while running, so the
# first install restarts Steam once (the headset's interface restarts
# briefly). RESTART_STEAM=0 skips that.
set -euo pipefail
src=$(cd "$(dirname "$0")" && pwd)
dir=$HOME/Applications/JustVideo
RESTART_STEAM=${RESTART_STEAM:-1}
[ -f "$src/just-video" ] || { echo "just-video is missing next to this script" >&2; exit 1; }

mkdir -p "$dir"
if [ "$src" != "$dir" ]; then
    install -m 755 "$src/just-video" "$dir/just-video"
    install -m 644 "$src/steam-shortcut.py" "$dir/steam-shortcut.py"
    rm -rf "$dir/art"
    cp -r "$src/art" "$dir/art"
    [ -f "$src/VERSION" ] && cp "$src/VERSION" "$dir/VERSION"
fi
echo "Installed Just Video $(cat "$dir/VERSION" 2>/dev/null) in $dir"

launcher="$dir/Just Video"
# Steam names the library entry after this file. Messages go to just-video.log
# (the previous run's to just-video.log.1): Steam doesn't keep them.
cat > "$launcher" <<'SH'
#!/bin/sh
dir=$(dirname "$0")
[ -f "$dir/just-video.log" ] && mv -f "$dir/just-video.log" "$dir/just-video.log.1"
exec "$dir/just-video" "$@" 2>>"$dir/just-video.log"
SH
chmod +x "$launcher" "$dir/just-video"
if grep -rqs --text "Applications/JustVideo/Just Video" ~/.local/share/Steam/userdata/*/config/shortcuts.vdf; then
    echo "Updated Just Video (already in the Steam library)."
else
    steamos-add-to-steam "$launcher"
    echo "Added Just Video to the Steam library (Non-Steam)."
    # Steam writes the new entry to shortcuts.vdf a moment later.
    for _ in $(seq 20); do
        grep -rqs --text "Applications/JustVideo/Just Video" \
            ~/.local/share/Steam/userdata/*/config/shortcuts.vdf && break
        sleep 1
    done
fi

# Mark the entry as a VR app (OpenVR = 1). Prints the value it found.
openvr() {
    python3 - "$1" <<'PY'
import glob, os, sys
mode, marker = sys.argv[1], b"Applications/JustVideo/Just Video"
for path in glob.glob(os.path.expanduser("~/.local/share/Steam/userdata/*/config/shortcuts.vdf")):
    data = bytearray(open(path, "rb").read())
    start = data.find(marker)
    flag = data.find(b"\x02OpenVR\x00", start)
    if start < 0 or flag < 0 or flag - start > 2000:
        continue
    at = flag + len(b"\x02OpenVR\x00")
    print(int.from_bytes(data[at:at + 4], "little"))
    if mode == "set":
        data[at:at + 4] = (1).to_bytes(4, "little")
        open(path, "wb").write(data)
PY
}
# Library art: copies it into config/grid; with --set-icon also sets the icon.
art() {
    python3 "$dir/steam-shortcut.py" "$dir/art" "$@" ||
        echo "Warning: couldn't install the Steam library art." >&2
}
if [ "$(openvr check | sort -u)" = 1 ] && python3 "$dir/steam-shortcut.py" "$dir/art" --icon-ok; then
    :
elif [ "$RESTART_STEAM" = 1 ]; then
    echo "Restarting Steam once to mark Just Video as a VR app and set its icon..."
    steam -shutdown >/dev/null 2>&1 || true
    for _ in $(seq 60); do pgrep -x steam >/dev/null || break; sleep 1; done
    openvr set >/dev/null
    pgrep -x steam >/dev/null || art --set-icon >/dev/null
    echo "Done. Steam restarts on its own; if it doesn't, restart the headset."
elif [ "$(openvr check | sort -u)" != 1 ]; then
    echo "Note: Just Video isn't marked as a VR app yet, so it starts in the background."
    echo "      Run again without RESTART_STEAM=0 to fix (restarts Steam once)."
fi
if pgrep -x steam >/dev/null; then
    art
else
    art --set-icon
fi
echo "Ready: Steam > Library > Non-Steam > Just Video"
