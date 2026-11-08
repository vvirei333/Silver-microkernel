#!/usr/bin/env bash
# Builds the freestanding Linux ABI test programs and installs them into
# disk.img.
#
#   tools/linux-test.sh [image]
#
# Requires a Linux AArch64 cross compiler (`aarch64-linux-gnu-gcc`, present in
# Debian's gcc-aarch64-linux-gnu package). Guest commands:
#
#   linuxrun /sfs/linux-hello   # one RX segment at 0x400000, exit 42
#   linuxrun /sfs/linux-rw      # text RX + data RW/BSS, exit 7
#
# The image is modified in place; `git checkout -- disk.img` restores the
# tracked copy after the test.
set -euo pipefail

cd "$(dirname "$0")/.."

CC="${LINUX_CC:-aarch64-linux-gnu-gcc}"
IMAGE="${1:-disk.img}"

mkdir -p build
build() {
    local name="$1"
    "$CC" -static -nostdlib -ffreestanding -O2 \
        -Wl,-T,"tests/linux/$name.ld" \
        -Wl,-z,max-page-size=4096 \
        -o "build/$name.elf" "tests/linux/$name.c"
}

build hello
build rw

python3 scripts/sfs_inject.py "$IMAGE" build/hello.elf linux-hello
python3 scripts/sfs_inject.py "$IMAGE" build/rw.elf linux-rw

# A real static libc binary (the SFS image is 1 MiB; all three together fit).
if "$CC" -static -O2 -o build/libc.elf tests/linux/libc.c 2>/dev/null; then
    printf 'readme!!' > build/readme.txt
    python3 scripts/sfs_inject.py "$IMAGE" build/readme.txt readme.txt
    python3 scripts/sfs_inject.py "$IMAGE" build/libc.elf linux-libc
    LIBC=" linuxrun /sfs/linux-libc"
else
    LIBC=""
    echo "note: no static libc for $CC; skipped the libc test" >&2
fi

python3 scripts/sfs_fsck.py "$IMAGE"
echo "installed: linuxrun /sfs/linux-hello  linuxrun /sfs/linux-rw$LIBC"
