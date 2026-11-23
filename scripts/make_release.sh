#!/usr/bin/env bash
# Builds the ConsoleOS release bundle for the version in Cargo.toml.
#
# Output (never committed, ignored by git):
#   dist/werewolfos-<version>/
#     silver_microkernel        the release kernel ELF (linux-abi included)
#     disk.img                  the SFS image it boots from
#     INSTALL.md                the install/quickstart guide
#     MANIFEST                  version, features, toolchain, build date
#     SHA256SUMS                checksums of the three files above
#   dist/werewolfos-<version>-consoleos-aarch64.tar.gz
#
# Usage:
#   scripts/make_release.sh                 build + package
#   scripts/make_release.sh --no-linux      package the console-only kernel
#   scripts/make_release.sh --smoke         also boot the artifacts in QEMU
#
# The script is deliberately dependency-free beyond cargo and tar: a release
# must be reproducible on a fresh machine without a Python environment.
set -euo pipefail

cd "$(dirname "$0")/.."

FEATURES="no_std,linux-abi"
SMOKE=0
for argument in "$@"; do
    case "$argument" in
        --no-linux) FEATURES="no_std" ;;
        --smoke) SMOKE=1 ;;
        *) echo "usage: $0 [--no-linux] [--smoke]" >&2; exit 1 ;;
    esac
done

# The version lives in Cargo.toml only; build.rs publishes it to the kernel and
# the userspace binaries (banner, uname, Linux utsname).
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
if [ -z "$VERSION" ]; then
    echo "could not read the version from Cargo.toml" >&2
    exit 1
fi

NAME="werewolfos-$VERSION-consoleos-aarch64"
OUT="dist/werewolfos-$VERSION"

echo "==> building Silver $VERSION ($FEATURES)"
cargo build --release --target aarch64-unknown-none \
    --no-default-features --features "$FEATURES"

rm -rf "$OUT"
mkdir -p "$OUT"

KERNEL="target/aarch64-unknown-none/release/silver_microkernel"
install -m 0644 "$KERNEL" "$OUT/silver_microkernel"
install -m 0644 disk.img "$OUT/disk.img"
install -m 0644 docs/INSTALL.md "$OUT/INSTALL.md"

RUSTC_VERSION="$(rustc --version 2>/dev/null || echo unknown)"
{
    echo "name: WerewolfOS ConsoleOS"
    echo "version: $VERSION"
    echo "kernel: Silver Microkernel $VERSION"
    echo "target: aarch64-unknown-none"
    echo "features: $FEATURES"
    echo "rustc: $RUSTC_VERSION"
    echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "files:"
    echo "  silver_microkernel  release kernel ELF, QEMU virt (Cortex-A72)"
    echo "  disk.img            64 MiB SFS image, boots the console OS"
    echo "  INSTALL.md          install and quickstart guide"
} > "$OUT/MANIFEST"

(
    cd "$OUT"
    sha256sum silver_microkernel disk.img INSTALL.md > SHA256SUMS
)

tar -czf "dist/$NAME.tar.gz" -C dist "$(basename "$OUT")"

echo "==> artifacts in $OUT/"
ls -l "$OUT"
echo "==> tarball: dist/$NAME.tar.gz"

if [ "$SMOKE" -eq 1 ]; then
    echo "==> booting the packaged artifacts in QEMU"
    python3 scripts/release_smoke.py \
        --kernel "$OUT/silver_microkernel" \
        --image "$OUT/disk.img"
fi
