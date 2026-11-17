#!/usr/bin/env python3
"""Resolve and download an Alpine aarch64 package closure without apk.

The Windows build machine has no qemu-user, so apk.static cannot run here.
This module fetches APKINDEX for the configured repositories, resolves the
dependency closure of the given roots and downloads the packages. The .apk
files are plain concatenated gzip tar members (signature, control, data) and
are unpacked by build_alpine_phosh_direct.py directly into the SFS image.

APKINDEX is a tar.gz whose single member is a text file of paragraph records;
`C` is the sha256 of the package payload, `P`/`V` the name/version, `D` the
depends, `p` the provides and `i` the install-if. Dependency strings may name
a package, `so:libc.so.6`, `cmd:sh` or `pc:...`; providers resolve those.
"""
import gzip
import hashlib
import io
import sys
import zlib
import tarfile
import time
import urllib.error
import urllib.request
from pathlib import Path

REPOSITORIES = (
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/main",
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/community",
)
ARCH = "aarch64"

UA = {"User-Agent": "werewolf-build/0.1"}


def _fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(request, timeout=300) as response:
        return response.read()


def fetch_index(repo: str) -> dict[str, dict]:
    """Parse APKINDEX into {package-name: record}."""
    blob = _fetch(f"{repo}/{ARCH}/APKINDEX.tar.gz")
    with tarfile.open(fileobj=io.BytesIO(blob), mode="r:gz") as archive:
        member = next(m for m in archive.getmembers() if m.name == "APKINDEX")
        text = archive.extractfile(member).read().decode("utf-8")
    packages: dict[str, dict] = {}
    for paragraph in text.split("\n\n"):
        record: dict[str, str] = {}
        for line in paragraph.splitlines():
            if ":" in line:
                key, value = line.split(":", 1)
                record[key] = value
        if "P" in record:
            record["_repo"] = repo
            packages[record["P"]] = record
    return packages


class Index:
    """Both repositories merged, with provider lookups."""

    def __init__(self) -> None:
        self.packages: dict[str, dict] = {}
        self.providers: dict[str, list[str]] = {}
        for repo in REPOSITORIES:
            for name, record in fetch_index(repo).items():
                existing = self.packages.get(name)
                # community wins over main only when newer; keep the first
                # and let the explicit depends order decide
                if existing is None or self._newer(record["V"], existing["V"]):
                    self.packages[name] = record
        for name, record in self.packages.items():
            for provides in record.get("p", "").split():
                self.providers.setdefault(provides.split("=")[0], []).append(name)

    @staticmethod
    def _newer(a: str, b: str) -> bool:
        return a > b  # good enough for one release

    def resolve_dep(self, dep: str) -> str | None:
        """Map one dependency token to a package name."""
        if dep.startswith("!"):
            return None  # conflicts: the closure simply omits both
        if ":" in dep and dep.split(":")[0] in ("so", "cmd", "pc", "dev"):
            head = dep.split("=", 1)[0]
            names = self.providers.get(head, [])
            if not names:
                sys.exit(f"no provider for {dep}")
            return names[0]
        base = dep.split("=")[0].split("<")[0].split(">")[0]
        for separator in ("~",):
            base = base.split(separator)[0]
        if base in self.packages:
            return base
        names = self.providers.get(base, [])
        if not names:
            sys.exit(f"no package satisfies {dep}")
        return names[0]

    def closure(self, roots: list[str]) -> list[dict]:
        """Dependency-ordered package list (dependencies first)."""
        seen: set[str] = set()
        ordered: list[dict] = []

        def visit(token: str) -> None:
            name = self.resolve_dep(token)
            if name is None or name in seen:
                return
            seen.add(name)
            record = self.packages[name]
            for dep in record.get("D", "").split():
                visit(dep)
            ordered.append(record)

        for root in roots:
            visit(root)
        return ordered


def download(record: dict, cache: Path) -> Path:
    repo = record["_repo"]
    name, version = record["P"], record["V"]
    path = cache / f"{name}-{version}.apk"
    if path.exists():
        return path
    print(f"fetch {name}-{version}")
    repos = [repo] + [r for r in REPOSITORIES if r != repo]
    blob = None
    for attempt in range(4):
        try:
            blob = _fetch(f"{repos[attempt % len(repos)]}/{ARCH}/{name}-{version}.apk")
            break
        except (urllib.error.HTTPError, TimeoutError, OSError) as error:
            retryable = (not isinstance(error, urllib.error.HTTPError)
                         or error.code == 404)
            if not retryable:
                raise
            print(f"  retry {name} ({error})")
            time.sleep(2.0 * (attempt + 1))
    if blob is None:
        sys.exit(f"404 for {name}-{version} in every repository after retries")
    path.write_bytes(blob)
    return path


def apk_data_members(path: Path) -> list[bytes]:
    """Split an .apk into its gzip members and return the decompressed tars.

    An apk is N concatenated gzip streams; each stream holds one tar. The
    last (largest) member is the payload.
    """
    raw = path.read_bytes()
    members = []
    offset = 0
    while offset < len(raw):
        decompressor = zlib.decompressobj(16 + zlib.MAX_WBITS)
        chunk = decompressor.decompress(raw[offset:])
        members.append(chunk)
        remaining = len(raw) - offset
        consumed = remaining - len(decompressor.unused_data)
        offset += consumed
        if not decompressor.unused_data:
            break
    return members


if __name__ == "__main__":
    index = Index()
    roots = sys.argv[1:] or ["alpine-base", "openrc", "phosh", "phoc", "dbus"]
    packages = index.closure(roots)
    print(f"{len(packages)} packages in the closure")
    for record in packages:
        print(f"  {record['P']}-{record['V']}")
