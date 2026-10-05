#!/usr/bin/env bash
# Installs this package (an unpacked release zip) on the Steam Frame over SSH,
# from Linux or macOS. Developer Mode must be on, with a user password set
# (Settings > System > Enable Developer Mode, then Developer > Set User
# Password). Without an SSH key, that password is asked for twice.
#
#   bash deploy.sh                 # the Frame as frame.local
#   bash deploy.sh 192.168.1.50    # or by its IP address
set -euo pipefail
src=$(cd "$(dirname "$0")" && pwd)
host=steamos@${1:-frame.local}
opts=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout=15)
# A fresh folder on the headset each time; removed after installing.
tmp=.cache/jv-install-$(date +%Y%m%d%H%M%S)

[ -f "$src/just-video" ] || { echo "just-video is missing next to this script" >&2; exit 1; }
echo "== Copying Just Video to ${host#*@} (password: the Frame's Developer Mode user password)"
if ! scp "${opts[@]}" -r "$src" "$host:$tmp"; then
    echo "Copying failed. Is the Frame on, awake, on the same network, with Developer Mode on?" >&2
    echo "Try its IP address instead: bash deploy.sh <ip>" >&2
    exit 1
fi
echo "== Installing (the first install restarts Steam on the Frame once)"
ssh "${opts[@]}" "$host" "bash ~/$tmp/install-on-frame.sh; rc=\$?; rm -rf ~/$tmp; exit \$rc"
echo "Done! On the Frame: Steam > Library > Non-Steam > Just Video"
