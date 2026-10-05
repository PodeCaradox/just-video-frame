"""Zips a folder for a release, keeping Unix file modes (the app and scripts
stay executable when unpacked with Ark or unzip on the headset).

Usage: python3 make-zip.py <folder> <out.zip>
The zip holds the folder itself (e.g. JustVideo/...), entries sorted.
"""

import os
import sys
import zipfile


def main(src: str, dst: str) -> None:
    src = os.path.abspath(src)
    base = os.path.dirname(src)
    with zipfile.ZipFile(dst, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for root, dirs, files in os.walk(src):
            dirs.sort()
            for name in sorted(files):
                path = os.path.join(root, name)
                info = zipfile.ZipInfo.from_file(path, os.path.relpath(path, base))
                info.compress_type = zipfile.ZIP_DEFLATED
                with open(path, "rb") as f:
                    z.writestr(info, f.read())


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
