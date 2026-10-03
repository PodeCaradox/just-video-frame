#!/usr/bin/env bash
# Installs the Steam Frame build into ~/Applications/JustVideo on the headset
# and adds "Just Video" to the Steam library as a non-Steam game (first run only).
# Build first: scripts/build-frame-media.sh (once), then scripts/build-frame.sh.
# The entry must be marked as a VR app, or SteamVR keeps its own menu in front
# and the app starts "in the background". Steam only reads that flag at startup
# and rewrites the file while running, so setting it needs one Steam restart:
# FRAME_RESTART_STEAM=1 does that (the headset's interface restarts briefly).
set -euo pipefail
cd "$(dirname "$0")/.."
binary=target/aarch64-unknown-linux-gnu/release/just-video
[ -x "$binary" ] || { echo "Build first: scripts/build-frame.sh" >&2; exit 1; }
host=steamos@${FRAME_HOST:-frame.local}
ssh_opts=(-i "$HOME/.ssh/steam_frame_ed25519" -o IdentitiesOnly=yes -o BatchMode=yes)
# The installed binary is kept as just-video.prev (to roll back: move it back).
ssh "${ssh_opts[@]}" "$host" 'mkdir -p ~/Applications/JustVideo && cd ~/Applications/JustVideo &&
    { [ ! -f just-video ] || cp -p just-video just-video.prev; }'
rsync -a -e "ssh ${ssh_opts[*]}" "$binary" "$host:Applications/JustVideo/just-video"
ssh "${ssh_opts[@]}" "$host" "RESTART_STEAM=${FRAME_RESTART_STEAM:-0} bash -s" <<'REMOTE'
set -euo pipefail
dir=$HOME/Applications/JustVideo
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
fi

# Mark the entry as a VR app (OpenVR = 1). Prints the value it found.
openvr() {
    python3 - "$1" "$2" <<'PY'
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
if [ "$(openvr check x)" = 1 ]; then
    :
elif [ "$RESTART_STEAM" = 1 ]; then
    echo "Restarting Steam to mark Just Video as a VR app…"
    steam -shutdown >/dev/null 2>&1 || true
    for _ in $(seq 60); do pgrep -x steam >/dev/null || break; sleep 1; done
    openvr set x >/dev/null
    echo "Marked. Steam restarts on its own; if it doesn't, restart the headset."
else
    echo "Note: Just Video isn't marked as a VR app yet, so it starts in the background."
    echo "      Run again with FRAME_RESTART_STEAM=1 to fix (restarts Steam once)."
fi
REMOTE
