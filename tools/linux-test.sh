#!/usr/bin/env bash
# Builds the freestanding Linux ABI test program and installs it into disk.img.
#
#   tools/linux-test.sh [image]
#
# Requires a Linux AArch64 cross compiler (`aarch64-linux-gnu-gcc`, present in
# Debian's gcc-aarch64-linux-gnu package). The guest command is:
#
#   linuxrun /sfs/linux-hello
#
# The image is modified in place; `git checkout -- disk.img` restores the
# tracked copy after the test.
set -euo pipefail

cd "$(dirname "$0")/.."

CC="${LINUX_CC:-aarch64-linux-gnu-gcc}"
IMAGE="${1:-disk.img}"
OUT="build/linux-hello.elf"

mkdir -p build
"$CC" -static -nostdlib -ffreestanding -O2 \
    -Wl,-T,tests/linux/link.ld \
    -Wl,-z,max-page-size=4096 \
    -o "$OUT" tests/linux/hello.c

python3 scripts/sfs_inject.py "$IMAGE" "$OUT" linux-hello
python3 scripts/sfs_fsck.py "$IMAGE"
echo "installed: linuxrun /sfs/linux-hello"
