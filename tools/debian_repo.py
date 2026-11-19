#!/usr/bin/env python3
"""Debian arm64 package fetcher/resolver for the glibc runtime the Denial
desktop needs.

Denial ships aarch64 builds as glibc binaries (PT_INTERP
/lib/ld-linux-aarch64.so.1, DT_NEEDED on libc.so.6/libgbm.so.1/libseat.so.1/
libudev.so.1/libinput.so.10/...). Alpine has no glibc, so the runtime stack
is staged from Debian trixie: this module resolves Depends closures against
the Packages index and unpacks .deb data tarballs (ar container + xz/zst
compressed tar).

Mirrors tools/alpine_repo.py usage:
    index = debian_repo.Index()
    records = index.closure(["libc6", "libgbm1", ...])
    path = debian_repo.download(record, cache)
"""
import gzip
import io
import lzma
import re
import tarfile
import urllib.request
from pathlib import Path

MIRROR = "https://deb.debian.org/debian"
SUITE = "trixie"
COMPONENTS = ("main",)
ARCH = "arm64"

UA = {"User-Agent": "werewolf-image-builder/0.1"}

_DEP_SPLIT = re.compile(r"\s*,\s*")
_ALT_SPLIT = re.compile(r"\s*\|\s*")
_DEP_NAME = re.compile(r"^([a-z0-9+.-]+)")


def _fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(request, timeout=300) as response:
        return response.read()


def _parse_dep_token(token: str) -> str:
    """'libc6 (>= 2.34)' / 'pkg:any' -> 'libc6'."""
    match = _DEP_NAME.match(token.strip())
    if not match:
        return ""
    return match.group(1)


def parse_index(gz_payload: bytes) -> dict[str, dict]:
    """Parse a Debian Packages file into {package: record}."""
    text = gzip.decompress(gz_payload).decode("utf-8", "replace")
    packages: dict[str, dict] = {}
    fields: dict[str, str] = {}
    key = None
    def flush():
        nonlocal fields
        if "Package" in fields:
            packages[fields["Package"]] = fields
        fields = {}
    for line in text.splitlines():
        if not line:
            flush()
            key = None
            continue
        if line.startswith((" ", "\t")) and key:
            fields[key] += "\n" + line.strip()
        elif ":" in line:
            key, _, value = line.partition(":")
            fields[key.strip()] = value.strip()
    flush()
    return packages


def ar_members(path: Path) -> dict[str, bytes]:
    """Split a .deb (ar archive) into named members."""
    payload = path.read_bytes()
    if payload[:8] != b"!<arch>\n":
        raise ValueError(f"{path}: not an ar archive")
    members: dict[str, bytes] = {}
    offset = 8
    while offset < len(payload):
        header = payload[offset:offset + 60]
        if len(header) < 60:
            break
        name = header[0:16].decode("ascii").rstrip().rstrip("/")
        size = int(header[48:58].decode("ascii").strip())
        body = payload[offset + 60:offset + 60 + size]
        offset += 60 + size
        if offset & 1:
            offset += 1
        members[name] = body
    return members


def deb_data_tar(path: Path) -> tarfile.TarFile:
    """Open the data.tar member of a .deb (fully decompressed, seekable)."""
    members = ar_members(path)
    data = next((body for name, body in members.items()
                 if name.startswith("data.tar")), None)
    if data is None:
        raise ValueError(f"{path}: no data.tar member")
    if data[:6] == b"fd7zXZ" or data[:2] == b"\xfd7":
        decompressed = lzma.decompress(data)
    elif data[:2] == b"\x1f\x8b":
        decompressed = gzip.decompress(data)
    elif data[:4] == b"\x28\xb5\x2f\xfd":
        import compression.zstd
        decompressed = compression.zstd.decompress(data)
    else:
        raise ValueError(f"{path}: unknown data.tar compression "
                         f"{data[:8].hex()}")
    return tarfile.open(fileobj=io.BytesIO(decompressed), mode="r:")


class Index:
    """Packages index for one Debian suite, with dependency resolution."""

    def __init__(self, suite: str = SUITE) -> None:
        self.records: dict[str, dict] = {}
        self.suite = suite
        for component in COMPONENTS:
            url = (f"{MIRROR}/dists/{suite}/{component}/"
                   f"binary-{ARCH}/Packages.gz")
            print(f"fetching {suite}/{component} Packages ({ARCH}) ...")
            self.records.update(parse_index(_fetch(url)))
        print(f"{len(self.records)} packages in the index")

    def resolve_dep(self, dep: str) -> str | None:
        """First alternative of a dependency clause that exists."""
        for alternative in _ALT_SPLIT.split(dep):
            name = _parse_dep_token(alternative)
            if name and name in self.records:
                return name
        return None

    def depends_of(self, record: dict) -> list[str]:
        clauses: list[str] = []
        for field in ("Pre-Depends", "Depends"):
            raw = record.get(field)
            if raw:
                clauses.extend(_DEP_SPLIT.split(raw))
        resolved = []
        for clause in clauses:
            name = self.resolve_dep(clause)
            if name:
                resolved.append(name)
        return resolved

    def closure(self, roots: list[str]) -> list[dict]:
        """Topologically ordered dependency closure of the roots."""
        missing = [root for root in roots if root not in self.records]
        if missing:
            raise SystemExit(f"unknown Debian packages: {missing}")
        order: list[str] = []
        seen: set[str] = set()
        def visit(name: str) -> None:
            if name in seen:
                return
            seen.add(name)
            record = self.records[name]
            for dep in self.depends_of(record):
                visit(dep)
            order.append(name)
        for root in roots:
            visit(root)
        return [self.records[name] for name in order]


def download(record: dict, cache: Path) -> Path:
    """Fetch a .deb into the cache and return its path."""
    filename = record["Filename"]
    destination = cache / Path(filename).name
    if destination.exists() and destination.stat().st_size > 0:
        return destination
    destination.parent.mkdir(parents=True, exist_ok=True)
    payload = _fetch(f"{MIRROR}/{filename}")
    destination.write_bytes(payload)
    return destination


if __name__ == "__main__":
    import sys
    index = Index()
    roots = sys.argv[1:] or ["libc6"]
    closure = index.closure(roots)
    print(f"{len(closure)} packages in closure of {roots}:")
    for record in closure:
        print(f"  {record['Package']} {record.get('Version', '?')}")
