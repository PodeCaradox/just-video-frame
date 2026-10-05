#!/usr/bin/env bash
# Keeps the Steam Frame from sleeping while it's tested over SSH: Steam
# suspends it after an hour without input, even on the charger.
# Holds a logind sleep/idle inhibitor on the headset for HOURS (default 4).
# It runs as a user service: polkit refuses a blocking inhibitor to an SSH
# session (no seat) but allows the user's service manager. Steam's suspend is
# then refused ("Access denied due to active block inhibitor"), but Steam
# stays in its sleep state until the headset is used: VR apps don't draw, and
# its web helper holds a second decoder session, so 8K won't open on the
# hardware decoder. (A virtual mouse doesn't count as use.)
#   scripts/frame-keep-awake.sh [HOURS]   start (or restart) the inhibitor
#   scripts/frame-keep-awake.sh stop      let it sleep again
#   scripts/frame-keep-awake.sh status
set -euo pipefail
cd "$(dirname "$0")/.."
unit=just-video-keep-awake
case "${1:-4}" in
stop)
    bash scripts/frame-ssh.sh "systemctl --user stop $unit 2>/dev/null; echo stopped" ;;
status)
    bash scripts/frame-ssh.sh "systemd-inhibit --list --no-pager | grep '$unit' || echo 'not held'" ;;
*)
    seconds=$(( ${1:-4} * 3600 ))
    bash scripts/frame-ssh.sh "systemctl --user stop $unit 2>/dev/null
        systemd-run --user --quiet --collect --unit=$unit \
            systemd-inhibit --who=$unit --why='Tests over SSH' --what=sleep:idle --mode=block \
            sleep $seconds
        sleep 1; systemd-inhibit --list --no-pager | grep '$unit' || { echo 'inhibitor not held' >&2; exit 1; }" ;;
esac
