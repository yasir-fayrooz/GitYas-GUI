#!/usr/bin/env python3
"""Archive formats the release needs, written portably so they can be built on Windows.

NTFS has no Unix permission bits or owners, so neither `tar` nor `zip` on Windows can mark the macOS / Linux
executables as executable. This writes every entry with an explicit mode and root ownership instead.

  pack.py icns <out.icns> <png>...                      macOS icon from square PNGs (32, 128, 256, 512 px ...)
  pack.py macos-app <out.app> <binary> <icns> <version> the GitYas.app bundle (unsigned)
  pack.py targz <out.tar.gz> <dir>                      <dir> as a top-level entry (the updater's .app.tar.gz)
  pack.py zip <out.zip> <dir>                           the same as a zip (first installs on macOS)
  pack.py deb <out.deb> <version> <binary> <assets-dir> the Debian package
  pack.py squashfs-list <out.list> <binary> <assets-dir> gensquashfs pack file for the AppImage (and stages it)

Run with `python -I` (scripts/release/*.sh do).
"""

import gzip
import io
import os
import shutil
import struct
import sys
import tarfile
import time
import zipfile
from pathlib import Path

IDENTIFIER = "com.gityas.GitYas"
PRODUCT = "GitYas"
BINARY = "gityas"
DESCRIPTION = "A native Git client"
MAINTAINER = "Yasir Fayrooz"
HOMEPAGE = "https://github.com/yasir-fayrooz/GitYas-GUI"
# keyring's Secret Service backend talks D-Bus (libdbus itself is linked statically); the folder picker uses the XDG
# desktop portal.
DEB_DEPENDS = "xdg-desktop-portal"

# Reproducible archives: one fixed timestamp for every entry (zip cannot store dates before 1980).
MTIME = 315532800


def walk(root: Path):
    """(path, path relative to root's parent with / separators) for root and everything below it, sorted."""
    yield root, root.name
    for path in sorted(root.rglob("*")):
        yield path, path.relative_to(root.parent).as_posix()


def tar_add(tar: tarfile.TarFile, name: str, mode: int, data: bytes | None = None):
    info = tarfile.TarInfo(name)
    info.mtime = MTIME
    info.uid = info.gid = 0
    info.uname = info.gname = "root"
    info.mode = mode
    if data is None:
        info.type = tarfile.DIRTYPE
        tar.addfile(info)
    else:
        info.size = len(data)
        tar.addfile(info, io.BytesIO(data))


def tar_tree(tar: tarfile.TarFile, root: Path, executables: set[str]):
    for path, rel in walk(root):
        if path.is_dir():
            tar_add(tar, rel, 0o755)
        else:
            tar_add(tar, rel, 0o755 if rel in executables else 0o644, path.read_bytes())


def write_gz(out: Path, payload: bytes):
    # mtime=0 and no file name in the gzip header: the same input gives the same bytes.
    with open(out, "wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=0, filename="") as gz:
        gz.write(payload)


# ---- macOS ---------------------------------------------------------------------------------------------------------

# icns element types holding PNG data, by pixel size. 16@2x / 32@2x / 128@2x / 256@2x reuse the larger sizes.
ICNS_TYPES = {32: [b"ic11"], 64: [b"ic12"], 128: [b"ic07"], 256: [b"ic08", b"ic13"], 512: [b"ic09", b"ic14"],
              1024: [b"ic10"]}


def png_size(data: bytes) -> int:
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit("error: not a PNG")
    width, height = struct.unpack(">II", data[16:24])
    if width != height:
        raise SystemExit(f"error: icon is {width}x{height}, not square")
    return width


def icns(out: Path, pngs: list[Path]):
    chunks = b""
    for png in pngs:
        data = png.read_bytes()
        for kind in ICNS_TYPES.get(png_size(data), []):
            chunks += kind + struct.pack(">I", 8 + len(data)) + data
    if not chunks:
        raise SystemExit("error: no icon of a size icns supports")
    out.write_bytes(b"icns" + struct.pack(">I", 8 + len(chunks)) + chunks)


def macos_app(out: Path, binary: Path, icon: Path, version: str):
    if out.exists():
        shutil.rmtree(out)
    (out / "Contents" / "MacOS").mkdir(parents=True)
    (out / "Contents" / "Resources").mkdir()
    shutil.copyfile(binary, out / "Contents" / "MacOS" / BINARY)
    shutil.copyfile(icon, out / "Contents" / "Resources" / f"{PRODUCT}.icns")
    (out / "Contents" / "Info.plist").write_text(f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>English</string>
  <key>CFBundleDisplayName</key><string>{PRODUCT}</string>
  <key>CFBundleExecutable</key><string>{BINARY}</string>
  <key>CFBundleIconFile</key><string>{PRODUCT}</string>
  <key>CFBundleIdentifier</key><string>{IDENTIFIER}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>{PRODUCT}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
""", newline="\n")


def app_executables(root: Path) -> set[str]:
    return {f"{root.name}/Contents/MacOS/{BINARY}"}


def targz(out: Path, root: Path):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as tar:
        tar_tree(tar, root, app_executables(root))
    write_gz(out, buffer.getvalue())


def zip_tree(out: Path, root: Path):
    executables = app_executables(root)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as archive:
        for path, rel in walk(root):
            date = time.gmtime(MTIME)[:6]
            if path.is_dir():
                info = zipfile.ZipInfo(rel + "/", date)
                info.external_attr = (0o040755 << 16) | 0x10
                archive.writestr(info, b"")
            else:
                info = zipfile.ZipInfo(rel, date)
                mode = 0o755 if rel in executables else 0o644
                info.external_attr = (0o100000 | mode) << 16
                info.create_system = 3  # Unix, so the mode above is honoured
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, path.read_bytes())


# ---- Linux ---------------------------------------------------------------------------------------------------------

DESKTOP_ENTRY = f"""[Desktop Entry]
Type=Application
Name={PRODUCT}
Comment={DESCRIPTION}
Exec={BINARY}
Icon={BINARY}
Categories=Development;RevisionControl;
Terminal=false
StartupWMClass={IDENTIFIER}
"""

# hicolor size directory -> source PNG in src-frontend/assets.
ICONS = {"32x32": "32x32.png", "128x128": "128x128.png", "256x256": "128x128@2x.png", "512x512": "icon.png"}


def linux_files(binary: Path, assets: Path) -> dict[str, tuple[int, bytes]]:
    """Path under / -> (mode, contents): what the .deb installs and the AppImage carries under usr/."""
    files = {
        f"usr/bin/{BINARY}": (0o755, binary.read_bytes()),
        f"usr/share/applications/{BINARY}.desktop": (0o644, DESKTOP_ENTRY.encode()),
    }
    for size, name in ICONS.items():
        files[f"usr/share/icons/hicolor/{size}/apps/{BINARY}.png"] = (0o644, (assets / name).read_bytes())
    return files


def parent_dirs(paths) -> list[str]:
    dirs = set()
    for path in paths:
        parts = path.split("/")[:-1]
        for i in range(1, len(parts) + 1):
            dirs.add("/".join(parts[:i]))
    return sorted(dirs)


def ar_member(name: str, data: bytes) -> bytes:
    header = f"{name:<16}{MTIME:<12}{0:<6}{0:<6}{0o100644:<8o}{len(data):<10}`\n".encode()
    return header + data + (b"\n" if len(data) % 2 else b"")


def tar_gz_bytes(entries: dict[str, tuple[int, bytes]], dirs: list[str]) -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.GNU_FORMAT) as tar:
        tar_add(tar, "./", 0o755)
        for directory in dirs:
            tar_add(tar, f"./{directory}/", 0o755)
        for path, (mode, data) in sorted(entries.items()):
            tar_add(tar, f"./{path}", mode, data)
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0, filename="") as gz:
        gz.write(buffer.getvalue())
    return out.getvalue()


def deb(out: Path, version: str, binary: Path, assets: Path):
    files = linux_files(binary, assets)
    installed_kb = sum(len(data) for _, data in files.values()) // 1024
    control = f"""Package: {BINARY}
Version: {version}
Architecture: amd64
Maintainer: {MAINTAINER}
Installed-Size: {installed_kb}
Depends: {DEB_DEPENDS}
Section: devel
Priority: optional
Homepage: {HOMEPAGE}
Description: {DESCRIPTION}
"""
    control_tar = tar_gz_bytes({"control": (0o644, control.encode())}, [])
    data_tar = tar_gz_bytes(files, parent_dirs(files))
    out.write_bytes(b"!<arch>\n" + ar_member("debian-binary", b"2.0\n") + ar_member("control.tar.gz", control_tar)
                    + ar_member("data.tar.gz", data_tar))


def squashfs_list(out: Path, binary: Path, assets: Path):
    """Stages the AppDir next to <out> and writes a gensquashfs pack file (-F) whose paths are relative to it (-D)."""
    files = linux_files(binary, assets)
    files[f"{BINARY}.desktop"] = files[f"usr/share/applications/{BINARY}.desktop"]
    files[f"{BINARY}.png"] = files[f"usr/share/icons/hicolor/256x256/apps/{BINARY}.png"]
    files[".DirIcon"] = files[f"{BINARY}.png"]
    stage = out.with_suffix(".appdir")
    if stage.exists():
        shutil.rmtree(stage)
    # No paths contain spaces, so nothing needs gensquashfs' quoting.
    lines = []
    for directory in parent_dirs(files):
        lines.append(f"dir /{directory} 0755 0 0")
    for path, (mode, data) in sorted(files.items()):
        source = stage / path
        source.parent.mkdir(parents=True, exist_ok=True)
        source.write_bytes(data)
        lines.append(f"file /{path} {mode:04o} 0 0 {path}")
    # The AppImage runtime starts AppRun; APPIMAGE / APPDIR are set by the runtime, which the updater reads.
    lines.append(f'slink "/AppRun" 0777 0 0 usr/bin/{BINARY}')
    out.write_text("\n".join(lines) + "\n", newline="\n")
    print(stage)


def main(argv: list[str]):
    if len(argv) < 2:
        raise SystemExit(__doc__)
    command, args = argv[1], [Path(a) for a in argv[2:]]
    if command == "icns" and len(args) >= 2:
        icns(args[0], args[1:])
    elif command == "macos-app" and len(args) == 4:
        macos_app(args[0], args[1], args[2], str(args[3]))
    elif command == "targz" and len(args) == 2:
        targz(args[0], args[1])
    elif command == "zip" and len(args) == 2:
        zip_tree(args[0], args[1])
    elif command == "deb" and len(args) == 4:
        deb(args[0], argv[3], args[2], args[3])
    elif command == "squashfs-list" and len(args) == 3:
        squashfs_list(args[0], args[1], args[2])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
