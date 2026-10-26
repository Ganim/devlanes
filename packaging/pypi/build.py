#!/usr/bin/env python3
"""Builds the PyPI wheels for VERSION from the release archives in DIST_DIR.

usage: build.py VERSION DIST_DIR OUT_DIR
       build.py --pep440 VERSION

Each wheel installs the release binaries into the environment's bin directory
(Scripts on Windows) through its .data/scripts directory, so devlanes-hosts
lands next to devlanes. --pep440 prints the PyPI version for a release version
and fails for one PyPI can't take. Standard library only.
"""

import base64
import hashlib
import io
import re
import sys
import tarfile
import time
import zipfile
from pathlib import Path

NAME = "devlanes"
HERE = Path(__file__).resolve().parent

# Release target, wheel platform tag. The macOS tags are Rust's default deployment
# targets; the Linux ones match the glibc 2.35 baseline of the Ubuntu 22.04 runners,
# which also keeps pip from picking them on musl.
TARGETS = [
    ("aarch64-apple-darwin", "macosx_11_0_arm64"),
    ("x86_64-apple-darwin", "macosx_10_12_x86_64"),
    ("aarch64-unknown-linux-gnu", "manylinux_2_35_aarch64"),
    ("x86_64-unknown-linux-gnu", "manylinux_2_35_x86_64"),
    ("x86_64-pc-windows-msvc", "win_amd64"),
    ("aarch64-pc-windows-msvc", "win_arm64"),
]

# PyPI only orders a, b and rc prereleases; Sampo starts a series without a number
# (1.2.0-rc, then 1.2.0-rc.1), which PEP 440 reads as rc0. An explicit .0 would
# collide with that, so it is refused.
PRERELEASE = re.compile(r"^(\d+\.\d+\.\d+)(?:-(alpha|beta|rc)(?:\.([1-9]\d*))?)?$")
PEP440_LABELS = {"alpha": "a", "beta": "b", "rc": "rc"}

CLASSIFIERS = [
    "Development Status :: 4 - Beta",
    "Environment :: Console",
    "Intended Audience :: Developers",
    "Programming Language :: Rust",
    "Topic :: Software Development",
]


def pep440(version):
    """Returns the PEP 440 form of a release version, or None when PyPI can't take it."""
    match = PRERELEASE.match(version)
    if not match:
        return None
    release, label, number = match.groups()
    if label is None:
        return release
    return f"{release}{PEP440_LABELS[label]}{int(number or 0)}"


def read_archive(path, prefix):
    """Returns the archive's files under PREFIX/ as {name: bytes} and the binaries' mtime."""
    files = {}
    mtime = None
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            for info in archive.infolist():
                if info.filename.startswith(prefix + "/") and not info.is_dir():
                    name = info.filename[len(prefix) + 1 :]
                    files[name] = archive.read(info)
                    if name.startswith(NAME):
                        mtime = info.date_time
    else:
        with tarfile.open(path, "r:gz") as archive:
            for member in archive.getmembers():
                if member.name.startswith(prefix + "/") and member.isfile():
                    name = member.name[len(prefix) + 1 :]
                    files[name] = archive.extractfile(member).read()
                    if name.startswith(NAME):
                        mtime = zip_time(member.mtime)
    return files, mtime


def zip_time(epoch):
    """Converts seconds since the epoch to a zip timestamp, which can't predate 1980."""
    return time.gmtime(max(epoch, 315532800))[:6]


def record_hash(data):
    """The RECORD form of a file's SHA-256: urlsafe base64 without padding."""
    digest = hashlib.sha256(data).digest()
    return "sha256=" + base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def metadata(version):
    """The wheel's METADATA, with the PyPI README as its description."""
    lines = [
        "Metadata-Version: 2.4",
        f"Name: {NAME}",
        f"Version: {version}",
        "Summary: Development environments in Git worktree lanes",
        "License-Expression: Apache-2.0",
        "License-File: LICENSE",
        "Project-URL: Homepage, https://devlanes.run",
        "Project-URL: Documentation, https://devlanes.run/docs/",
        "Project-URL: Source, https://github.com/devlanes-run/devlanes",
        "Project-URL: Changelog, https://github.com/devlanes-run/devlanes/blob/main/bins/devlanes/CHANGELOG.md",
        *(f"Classifier: {classifier}" for classifier in CLASSIFIERS),
        "Description-Content-Type: text/markdown",
    ]
    readme = (HERE / "README.md").read_text(encoding="utf-8")
    return ("\n".join(lines) + "\n\n" + readme).encode()


def build_wheel(version, files, mtime, platform, out):
    """Writes one wheel for PLATFORM holding the binaries and LICENSE in FILES."""
    data = f"{NAME}-{version}.data/scripts"
    info = f"{NAME}-{version}.dist-info"
    wheel = (
        "Wheel-Version: 1.0\n"
        "Generator: devlanes packaging/pypi/build.py\n"
        "Root-Is-Purelib: false\n"
        f"Tag: py3-none-{platform}\n"
    ).encode()
    entries = [
        (f"{data}/{name}", files[name], 0o755)
        for name in sorted(files)
        if name != "LICENSE"
    ]
    entries += [
        (f"{info}/METADATA", metadata(version), 0o644),
        (f"{info}/WHEEL", wheel, 0o644),
        (f"{info}/licenses/LICENSE", files["LICENSE"], 0o644),
    ]
    record = "".join(f"{name},{record_hash(body)},{len(body)}\n" for name, body, _ in entries)
    entries.append((f"{info}/RECORD", (record + f"{info}/RECORD,,\n").encode(), 0o644))

    path = out / f"{NAME}-{version}-py3-none-{platform}.whl"
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, body, mode in entries:
            entry = zipfile.ZipInfo(name, mtime)
            entry.compress_type = zipfile.ZIP_DEFLATED
            # Unix file type and mode in the high bits; pip and uv restore the
            # executable bit from them.
            entry.external_attr = (0o100000 | mode) << 16
            entry.create_system = 3
            archive.writestr(entry, body)
    path.write_bytes(buffer.getvalue())
    return path


def build(version, dist, out):
    """Builds every target's wheel from the release archives in DIST into OUT."""
    wheel_version = pep440(version)
    if wheel_version is None:
        sys.exit(f"{version} has no PEP 440 form; only X.Y.Z and -alpha, -beta or -rc prereleases go to PyPI")
    if out.exists():
        sys.exit(f"{out} already exists")
    out.mkdir(parents=True)
    for target, platform in TARGETS:
        prefix = f"{NAME}-{version}-{target}"
        windows = "windows" in target
        archive = dist / (prefix + (".zip" if windows else ".tar.gz"))
        if not archive.is_file():
            sys.exit(f"missing {archive}")
        files, mtime = read_archive(archive, prefix)
        expected = {"devlanes.exe", "LICENSE"} if windows else {"devlanes", "devlanes-hosts", "LICENSE"}
        if set(files) != expected:
            sys.exit(f"{archive} holds {sorted(files)}, expected {sorted(expected)}")
        print(build_wheel(wheel_version, files, mtime, platform, out))


def main(args):
    """Parses the command line."""
    if len(args) == 2 and args[0] == "--pep440":
        version = pep440(args[1])
        if version is None:
            sys.exit(f"{args[1]} has no PEP 440 form")
        print(version)
    elif len(args) == 3 and not args[0].startswith("-"):
        build(args[0], Path(args[1]), Path(args[2]))
    else:
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main(sys.argv[1:])
