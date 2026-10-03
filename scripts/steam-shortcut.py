#!/usr/bin/env python3
"""Gives the "Just Video" non-Steam shortcut its library artwork.

Run on the headset by install-frame.sh. For every Steam user whose
shortcuts.vdf has the Just Video entry, copies the art (made by
`just-video steam-art`) into config/grid/ under the shortcut's app id:

  portrait.png -> <id>p.png      (library capsule, the "poster")
  capsule.png  -> <id>.png       (wide capsule)
  hero.png     -> <id>_hero.png  (banner on the game page)
  icon.png     -> <id>_icon.png

  steam-shortcut.py ART_DIR              copy the grid art (safe while Steam runs)
  steam-shortcut.py ART_DIR --set-icon   also point the entry's icon at ART_DIR/icon.png
  steam-shortcut.py ART_DIR --icon-ok    exit 1 if the icon isn't set yet

Steam keeps shortcuts.vdf in memory and rewrites it while running, so
--set-icon must only run while Steam is stopped. STEAM_USERDATA overrides
~/.local/share/Steam/userdata (for testing).
"""

import glob
import os
import re
import shutil
import sys
import zlib

MARKER = b"Applications/JustVideo/Just Video"
GRID_NAMES = {
    "portrait.png": "{}p.png",
    "capsule.png": "{}.png",
    "hero.png": "{}_hero.png",
    "icon.png": "{}_icon.png",
}

# Binary VDF: each field is <type byte><key>\0<value>; a map ends with 0x08.
MAP, STRING, INT32, END = 0, 1, 2, 8


def parse(data, pos=0):
    """Parses a map body into a list of [type, key, value]; returns (items, end)."""
    items = []
    while True:
        kind = data[pos]
        pos += 1
        if kind == END:
            return items, pos
        end = data.index(b"\0", pos)
        key, pos = data[pos:end], end + 1
        if kind == MAP:
            value, pos = parse(data, pos)
        elif kind == STRING:
            end = data.index(b"\0", pos)
            value, pos = data[pos:end], end + 1
        elif kind == INT32:
            value, pos = data[pos : pos + 4], pos + 4
        else:
            raise ValueError(f"unknown VDF field type {kind:#x} at {pos - 1}")
        items.append([kind, key, value])


def serialize(items):
    out = bytearray()
    for kind, key, value in items:
        out += bytes([kind]) + key + b"\0"
        if kind == MAP:
            out += serialize(value)
        elif kind == STRING:
            out += value + b"\0"
        else:
            out += value
    return bytes(out + bytes([END]))


def field(entry, name):
    """The [type, key, value] item named `name` (keys vary in case: AppName/appname)."""
    for item in entry:
        if item[1].lower() == name:
            return item
    return None


def legacy_id(exe, appname):
    """Grid id of a shortcut without a stored appid (older Steam).

    Steam derived it from the Exe string exactly as stored (with its quotes)
    followed by the name: top = crc32(exe + appname) | 0x80000000. The 64-bit
    game id is (top << 32) | 0x02000000; grid files use the 32-bit `top`.
    Matches Steam ROM Manager's generate-app-id.ts (its "short id" is the
    same `top` as a signed int32, which is how the appid field stores it).
    Steam versions that store `appid` (all current ones) don't need this.
    """
    return zlib.crc32(exe + appname) | 0x80000000


def entry_id(entry):
    appid = field(entry, b"appid")
    if appid and appid[0] == INT32:
        return int.from_bytes(appid[2], "little")  # unsigned, as grid names use
    exe, name = field(entry, b"exe"), field(entry, b"appname")
    return legacy_id(exe[2], name[2] if name else b"")


def find_entries(items):
    shortcuts = field(items, b"shortcuts")
    if not shortcuts or shortcuts[0] != MAP:
        return []
    found = []
    for kind, _, entry in shortcuts[2]:
        exe = field(entry, b"exe") if kind == MAP else None
        if exe and exe[0] == STRING and MARKER in exe[2]:
            found.append(entry)
    return found


def fallback_id(data):
    """Byte-level appid lookup when the file doesn't parse cleanly.

    The appid field comes before Exe within an entry, so search backward from
    the marker, but only as far as the start of this entry (its index key,
    `\\0<n>\\0`, right after the previous entry's end or the "shortcuts" key);
    otherwise we could pick up the previous shortcut's appid.
    """
    at = data.find(MARKER)
    if at < 0:
        return None
    starts = list(re.finditer(rb"(?:\x08|\x00shortcuts)\x00\d+\x00", data[:at], re.I))
    if not starts:
        return None
    entry = data[starts[-1].end() : at]
    flag = entry.lower().rfind(b"\x02appid\x00")
    if flag < 0:
        return None
    at = flag + len(b"\x02appid\x00")
    return int.from_bytes(entry[at : at + 4], "little")


def write_atomic(path, data):
    backup = path + ".justvideo.bak"
    if not os.path.exists(backup):
        shutil.copy2(path, backup)
    tmp = path + ".justvideo.tmp"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, path)


def set_icon(path, items, entries, icon):
    for entry in entries:
        item = field(entry, b"icon")
        if item:
            item[0], item[2] = STRING, icon
        else:
            entry.append([STRING, b"icon", icon])
    new = serialize(items)
    # Sanity check before replacing the user's library file.
    reparsed, end = parse(new)
    if end != len(new) or len(find_entries(reparsed)) != len(entries):
        raise ValueError("edited file doesn't parse back")
    write_atomic(path, new)
    print(f"Set the shortcut icon in {path}.")


def main():
    art = os.path.abspath(sys.argv[1])
    mode = sys.argv[2] if len(sys.argv) > 2 else ""
    icon = os.path.join(art, "icon.png").encode()
    root = os.environ.get("STEAM_USERDATA") or os.path.expanduser(
        "~/.local/share/Steam/userdata"
    )
    seen, icons_ok = False, True
    for path in sorted(glob.glob(os.path.join(root, "*", "config", "shortcuts.vdf"))):
        data = open(path, "rb").read()
        try:
            items, end = parse(data)
            if end != len(data) or serialize(items) != data:
                raise ValueError("doesn't round-trip")
            entries = find_entries(items)
            ids = [entry_id(e) for e in entries]
        except (ValueError, IndexError) as err:
            print(f"warning: {path}: {err}; not editing it", file=sys.stderr)
            items, entries = None, []
            ids = [i for i in [fallback_id(data)] if i is not None]
        if not ids:
            continue
        seen = True
        icon_ok = bool(entries) and all(
            (f := field(e, b"icon")) is not None and f[2] == icon for e in entries
        )
        if mode == "--icon-ok":
            icons_ok &= icon_ok or not entries
            continue
        grid = os.path.join(os.path.dirname(path), "grid")
        os.makedirs(grid, exist_ok=True)
        for gid in sorted(set(ids)):
            for src, name in GRID_NAMES.items():
                shutil.copyfile(os.path.join(art, src), os.path.join(grid, name.format(gid)))
            print(f"Steam art installed for app id {gid} ({grid}).")
        if mode == "--set-icon" and entries and not icon_ok:
            set_icon(path, items, entries, icon)
    if mode == "--icon-ok":
        return 0 if icons_ok else 1
    if not seen:
        print("Just Video isn't in shortcuts.vdf yet: run the install again to add its art.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
