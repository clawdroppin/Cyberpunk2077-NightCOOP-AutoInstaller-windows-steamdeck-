"""Collects the release builds into dist/ with portable names, a Steam Deck tarball and checksums."""

import hashlib
import io
import shutil
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"
WIN = ROOT / "target" / "release" / "nightcoop-helper.exe"
LINUX = ROOT / "target" / "x86_64-unknown-linux-gnu" / "release" / "nightcoop-helper"

DECK_README = """NightCoop Helper for Steam Deck
================================

1. Switch to Desktop Mode (Steam button > Power > Switch to Desktop).
2. Extract this archive anywhere, e.g. your Downloads folder
   (right-click > Extract > Extract archive here).
3. Double-click "NightCoopHelper". If it doesn't start: right-click it >
   Properties > Permissions > tick "Is executable", then try again.
4. Press "Install everything". Steam restarts once to save the launch option.
5. Return to Gaming Mode and start Cyberpunk 2077.

The helper keeps its settings and save backups in the
"NightCoopHelper-data" folder next to it. Move both together.
"""


def sha256(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def main() -> int:
    # File names carry no version (it lives in the release tag), so
    # .../releases/latest/download/<name> links never go stale.
    if DIST.exists():
        shutil.rmtree(DIST)
    DIST.mkdir()
    made = []
    if WIN.is_file():
        out = DIST / "NightCoopHelper-windows-x64.exe"
        shutil.copy2(WIN, out)
        made.append(out)
    if LINUX.is_file():
        raw = DIST / "NightCoopHelper-linux-x86_64"
        shutil.copy2(LINUX, raw)
        made.append(raw)
        # A tarball keeps the executable bit, which a plain download loses.
        tar_path = DIST / "NightCoopHelper-SteamDeck.tar.gz"
        with tarfile.open(tar_path, "w:gz") as tar:
            info = tar.gettarinfo(str(LINUX), arcname="NightCoopHelper/NightCoopHelper")
            info.mode = 0o755
            info.uid = info.gid = 1000
            info.uname = info.gname = "deck"
            with open(LINUX, "rb") as f:
                tar.addfile(info, f)
            data = DECK_README.encode()
            readme = tarfile.TarInfo("NightCoopHelper/README.txt")
            readme.size = len(data)
            readme.mode = 0o644
            tar.addfile(readme, io.BytesIO(data))
        made.append(tar_path)
    if not made:
        print("nothing to package: build first", file=sys.stderr)
        return 1
    sums = "".join(f"{sha256(p)}  {p.name}\n" for p in made)
    (DIST / "SHA256SUMS.txt").write_text(sums)
    for p in made:
        print(f"{p.stat().st_size / 1048576:6.1f} MB  {p.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
