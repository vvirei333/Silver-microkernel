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
#   linuxrun /sfs/linux-chain   # execve's linux-hello, exit 42
#   linuxrun /sfs/linux-fork    # fork+exec+wait4, parent exits 0
#   linuxrun /sfs/linux-pipe    # pipe2+ppoll+fork, prints 'pipe: ping'
#   linuxrun /sfs/linux-ipc     # eventfd2+memfd+futex, prints 'ipc ok'
#   linuxrun /sfs/linux-signal  # sigaction/sigprocmask/kill/SIGCHLD
#   linuxrun /sfs/linux-pie     # ET_DYN + PT_INTERP, prints 'pie main', exit 7
#   linuxrun /sfs/linux-bigfile # 64 MiB rootfs + 8 MiB file, prints 'rootfs ok'
#   linuxrun /sfs/linux-mmapfile # file mmap + mprotect + MAP_FIXED, prints 'mmap ok'
#   linuxrun /sfs/linux-sighandler # user handler + rt_sigframe/rt_sigreturn, prints 'sigframe ok'
#   linuxrun /sfs/linux-asyncsig  # IRQ-delivered signal + sigaltstack, prints 'async sig ok'
#   linuxrun /sfs/linux-cowfork   # COW fork isolation, prints 'cow fork ok'
#   linuxrun /sfs/linux-scope     # only /sfs is readable, prints 'scope ok'
#   linuxrun /sfs/linux-threads   # CLONE_VM + TLS + futex, prints 'threads ok'
#   linuxrun /sfs/linux-unixsock  # AF_UNIX + SCM_RIGHTS + EOF, prints 'unix socket ok'
#   linuxrun /sfs/linux-dyn     # real ld.so + dynamic libc, prints 'dynamic hello'
#   linuxrun /sfs/linux-dynsig  # glibc sigaction + raise, prints 'dyn signal 10'
#   linuxrun /sfs/linux-pthread # glibc pthread_create/join, prints 'pthread 42'
#   linuxrun /sfs/linux-pthread-exit # exit_group kills a live worker
#   linuxrun /sfs/linux-groupsig # signal dispositions shared by the group
#   linuxrun /sfs/linux-exitgroup # exit_group from a worker kills the process
#   linuxrun /sfs/linux-sockets # glibc AF_UNIX + SCM_RIGHTS, prints 'glibc socket ok'
#   linuxrun /sfs/linux-memfd  # memfd + MAP_SHARED across fork, prints 'shared memfd ok'
#   linuxrun /sfs/linux-namedsock # bind/listen/accept/connect, prints 'named socket ok'
#   linuxrun /sfs/linux-scratch # writable /tmp + mkdir/unlink/rename, prints 'scratch ok'
#   linuxrun /sfs/linux-eventloop # epoll + timerfd + eventfd, prints 'event loop ok'
#   linuxrun /sfs/linux-wayland # Wayland wire protocol + shm memfd, prints 'wayland ok'
#   linuxrun /sfs/linux-wlreal  # real libwayland-client against the mini compositor
#   linuxrun /sfs/linux-wlinfo  # stock wayland-info binary against the compositor
#   linuxrun /sfs/linux-dbus   # EXTERNAL auth + Hello/Reply, prints 'dbus ok'
#   linuxrun /sfs/glibtest      # real GLib 2.90 + GObject main loop
#   linuxrun /sfs/giotest       # real GIO file I/O + main loop
#   linuxrun /sfs/linux-libc    # static glibc: stdio/files/dirs, exit 0
#                               # (only when the cross compiler has libc.a)
#
# The image is modified in place. It is git-tracked; on first use the script
# grows the committed base to 16 MiB (scripts/sfs_resize.py preserves the
# existing tree), then installs the test programs. `git checkout -- disk.img`
# restores the clean base afterwards.
set -euo pipefail

cd "$(dirname "$0")/.."

CC="${LINUX_CC:-aarch64-linux-gnu-gcc}"
IMAGE="${1:-disk.img}"

# The kernel keeps the whole SFS in RAM. Grow the rootfs (preserving files)
# before installing tests; it now defaults to 64 MiB / 2048 inodes.
if [ ! -f "$IMAGE" ]; then
    python3 scripts/make_sfs.py "$IMAGE" 64 2048
fi
python3 scripts/sfs_resize.py "$IMAGE" 64 2048

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
build chain
build fork
build pipe
build ipc
build signal
build bigfile
build mmapfile
build sighandler
build asyncsig
build cowfork
build scope
build threads
build unixsock
build namedsock
build wayland
build wayland_real
build wayland_info
build dbus

# Real GLib/GObject (dynamically linked against the vendored aarch64 libs).
GLIB=third_party/glib-aarch64
if [ -f "$GLIB/lib/libglib-2.0.so.0" ] && [ -f "$GLIB/lib/libgobject-2.0.so.0" ]; then
    "$CC" -O2 -I"$GLIB/include/glib-2.0" -I"$GLIB/lib/glib-2.0/include" \
        -o build/glibtest tests/linux/glib.c \
        -L"$GLIB/lib" -Lthird_party/wayland-aarch64/lib \
        -Wl,-rpath-link,"$GLIB/lib" \
        -Wl,-rpath-link,third_party/wayland-aarch64/lib \
        -l:libglib-2.0.so.0 -l:libgobject-2.0.so.0 -l:libffi.so.8
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/glibtest
    "$CC" -O2 -I"$GLIB/include/glib-2.0" -I"$GLIB/lib/glib-2.0/include" \
        -o build/giotest tests/linux/gio.c \
        -L"$GLIB/lib" -Lthird_party/wayland-aarch64/lib \
        -Wl,-rpath-link,"$GLIB/lib" \
        -Wl,-rpath-link,third_party/wayland-aarch64/lib \
        -l:libgio-2.0.so.0 -l:libgobject-2.0.so.0 -l:libglib-2.0.so.0 \
        -l:libgmodule-2.0.so.0 -l:libffi.so.8
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/giotest
fi

# ET_DYN main + custom interpreter (no libc needed).
"$CC" -nostdlib -fPIE -pie -Wl,-e,_start \
    -Wl,--dynamic-linker=/sfs/ld-test -Wl,-z,max-page-size=4096 \
    -o build/pie-main.elf tests/linux/piemain.c
"$CC" -nostdlib -ffreestanding -static -O2 \
    -Wl,-T,tests/linux/chain.ld -Wl,-z,max-page-size=4096 \
    -o build/ld-test.elf tests/linux/interp.c

python3 scripts/sfs_inject.py "$IMAGE" build/hello.elf linux-hello
python3 scripts/sfs_inject.py "$IMAGE" build/rw.elf linux-rw
python3 scripts/sfs_inject.py "$IMAGE" build/chain.elf linux-chain
python3 scripts/sfs_inject.py "$IMAGE" build/fork.elf linux-fork
python3 scripts/sfs_inject.py "$IMAGE" build/pipe.elf linux-pipe
python3 scripts/sfs_inject.py "$IMAGE" build/ipc.elf linux-ipc
python3 scripts/sfs_inject.py "$IMAGE" build/signal.elf linux-signal
python3 scripts/sfs_inject.py "$IMAGE" build/pie-main.elf linux-pie
python3 scripts/sfs_inject.py "$IMAGE" build/ld-test.elf ld-test

# An 8 MiB payload proves the enlarged rootfs end to end (mount, VFS limits,
# 64 KiB reads). Byte i is `i & 0xff`; tests/linux/bigfile.c verifies all of it.
python3 - <<'PY'
from pathlib import Path
block = bytes(range(256)) * 4096
Path("build/big.bin").write_bytes((block * 8)[:8 * 1024 * 1024])
PY
python3 scripts/sfs_inject.py "$IMAGE" build/big.bin big.bin
python3 scripts/sfs_inject.py "$IMAGE" build/bigfile.elf linux-bigfile
python3 scripts/sfs_inject.py "$IMAGE" build/mmapfile.elf linux-mmapfile
python3 scripts/sfs_inject.py "$IMAGE" build/sighandler.elf linux-sighandler
python3 scripts/sfs_inject.py "$IMAGE" build/asyncsig.elf linux-asyncsig
python3 scripts/sfs_inject.py "$IMAGE" build/cowfork.elf linux-cowfork
python3 scripts/sfs_inject.py "$IMAGE" build/scope.elf linux-scope
python3 scripts/sfs_inject.py "$IMAGE" build/threads.elf linux-threads
python3 scripts/sfs_inject.py "$IMAGE" build/unixsock.elf linux-unixsock
python3 scripts/sfs_inject.py "$IMAGE" build/namedsock.elf linux-namedsock
python3 scripts/sfs_inject.py "$IMAGE" build/wayland.elf linux-wayland
python3 scripts/sfs_inject.py "$IMAGE" build/wayland_real.elf linux-wlreal
"$CC" -static -nostdlib -ffreestanding -O2 -DSERVE_ONLY -DSERVE_LAYER \
    -Wl,-T,tests/linux/wayland_real.ld -Wl,-z,max-page-size=4096 \
    -o build/layer_serve.elf tests/linux/wayland_real.c
python3 scripts/sfs_inject.py "$IMAGE" build/layer_serve.elf layer-serve
"$CC" -static -nostdlib -ffreestanding -O2 -DSERVE_ONLY -DSERVE_HIT \
    -Wl,-T,tests/linux/wayland_real.ld -Wl,-z,max-page-size=4096 \
    -o build/layer_hit.elf tests/linux/wayland_real.c
python3 scripts/sfs_inject.py "$IMAGE" build/layer_hit.elf layer-hit
"$CC" -static -nostdlib -ffreestanding -O2 \
    -Wl,-T,tests/linux/wayland_real.ld -Wl,-z,max-page-size=4096 \
    -o build/drmtest.elf tests/linux/drmtest.c
python3 scripts/sfs_inject.py "$IMAGE" build/drmtest.elf drmtest
"$CC" -static -nostdlib -ffreestanding -O2 \
    -Wl,-T,tests/linux/wayland_real.ld -Wl,-z,max-page-size=4096 \
    -o build/rendertest.elf tests/linux/rendertest.c
python3 scripts/sfs_inject.py "$IMAGE" build/rendertest.elf rendertest
python3 scripts/sfs_inject.py "$IMAGE" build/wayland_info.elf linux-wlinfo
python3 scripts/sfs_inject.py "$IMAGE" build/dbus.elf linux-dbus
if [ -f build/glibtest ]; then
    for library in libglib-2.0.so.0 libgobject-2.0.so.0 libpcre2-8.so.0 \
                   libgio-2.0.so.0 libgmodule-2.0.so.0 libz.so.1 \
                   libmount.so.1 libblkid.so.1 libsystemd.so.0 libgcc_s.so.1; do
        python3 scripts/sfs_inject.py "$IMAGE" "$GLIB/lib/$library" "$library"
    done
    python3 scripts/sfs_inject.py "$IMAGE" build/glibtest glibtest
    python3 scripts/sfs_inject.py "$IMAGE" build/giotest giotest
    GLIBTEST=" linuxrun /sfs/glibtest linuxrun /sfs/giotest"
else
    GLIBTEST=""
fi

# `close_range(3, ~0U, 0)` is what GLib does in a spawned child. Walking
# every descriptor to u32::MAX froze the nested-compositor session.
if "$CC" -static -O2 -o build/closerangeprobe tests/linux/closerangeprobe.c; then
    python3 scripts/sfs_inject.py "$IMAGE" build/closerangeprobe linux-closerange
    CLOSE_RANGE=" linuxrun /sfs/linux-closerange"
else
    CLOSE_RANGE=""
fi

# A real static libc binary (the SFS image is 64 MiB; all of these fit).
if "$CC" -static -O2 -o build/libc.elf tests/linux/libc.c 2>/dev/null; then
    printf 'readme!!' > build/readme.txt
    python3 scripts/sfs_inject.py "$IMAGE" build/readme.txt readme.txt
    python3 scripts/sfs_inject.py "$IMAGE" build/libc.elf linux-libc
    LIBC=" linuxrun /sfs/linux-libc"
else
    LIBC=""
    echo "note: no static libc for $CC; skipped the libc test" >&2
fi

# A dynamically linked binary exercises the real ld.so path: file-backed
# mmap, MAP_FIXED over a PROT_NONE reservation, and mprotect on the main
# image's RELRO. The interpreter is patched to the path in our VFS.
LD_SO="$("$CC" -print-file-name=ld-linux-aarch64.so.1)"
LIBC_SO="$("$CC" -print-file-name=libc.so.6)"
if command -v patchelf >/dev/null 2>&1 && [ -f "$LD_SO" ] && [ -f "$LIBC_SO" ] \
    && "$CC" -O2 -o build/dyn-hello tests/linux/dynhello.c 2>/dev/null; then
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/dyn-hello
    "$CC" -O2 -o build/dyn-signal tests/linux/dynsignal.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/dyn-signal
    python3 scripts/sfs_inject.py "$IMAGE" "$LD_SO" ld-linux-aarch64.so.1
    python3 scripts/sfs_inject.py "$IMAGE" "$LIBC_SO" libc.so.6
    "$CC" -O2 -pthread -o build/pthread-test tests/linux/pthread.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/pthread-test
    "$CC" -O2 -pthread -o build/pthread-exit tests/linux/pthread_exit.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/pthread-exit
    "$CC" -O2 -pthread -o build/group-signal tests/linux/group_signal.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/group-signal
    "$CC" -O2 -pthread -o build/exitgroup-thread tests/linux/exitgroup_thread.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/exitgroup-thread
    "$CC" -O2 -o build/sockets tests/linux/sockets.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/sockets
    "$CC" -O2 -o build/memfd-shared tests/linux/memfd_shared.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/memfd-shared
    "$CC" -O2 -pthread -o build/threadfork tests/linux/threadfork.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/threadfork
    "$CC" -O2 -pthread -o build/futexping tests/linux/futexping.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/futexping
    "$CC" -O2 -o build/scratch tests/linux/scratch.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/scratch
    "$CC" -O2 -o build/eventloop tests/linux/eventloop.c
    patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
        --set-rpath '$ORIGIN' build/eventloop
    WAYLAND=third_party/wayland-aarch64
    if [ -f "$WAYLAND/lib/libwayland-client.so" ] && [ -f "$WAYLAND/lib/libffi.so.8" ]; then
        "$CC" -O2 -I"$WAYLAND/include" -o build/wlclient tests/linux/wlclient.c \
            "$WAYLAND/lib/xdg-shell-protocol.c" \
            "$WAYLAND/lib/xdg-decoration-protocol.c" \
            "$WAYLAND/lib/viewporter-protocol.c" \
            "$WAYLAND/lib/xdg-output-protocol.c" \
            "$WAYLAND/lib/xdg-activation-protocol.c" \
            "$WAYLAND/lib/fractional-scale-protocol.c" \
            "$WAYLAND/lib/presentation-time-protocol.c" \
            "$WAYLAND/lib/relative-pointer-protocol.c" \
            "$WAYLAND/lib/pointer-constraints-protocol.c" \
            "$WAYLAND/lib/text-input-protocol.c" \
            "$WAYLAND/lib/wlr-layer-shell-protocol.c" \
            -L"$WAYLAND/lib" -Wl,-rpath-link,"$WAYLAND/lib" \
            -l:libwayland-client.so -l:libffi.so.8
        patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
            --set-rpath '$ORIGIN' build/wlclient
        python3 scripts/sfs_inject.py "$IMAGE" "$WAYLAND/lib/libwayland-client.so" libwayland-client.so.0
        python3 scripts/sfs_inject.py "$IMAGE" "$WAYLAND/lib/libffi.so.8" libffi.so.8
        python3 scripts/sfs_inject.py "$IMAGE" "$WAYLAND/lib/libdrm.so.2" libdrm.so.2
        cp "$WAYLAND/bin/wayland-info" build/wayland-info
        patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
            --set-rpath '$ORIGIN' build/wayland-info
        python3 scripts/sfs_inject.py "$IMAGE" build/wayland-info wayland-info
        python3 scripts/sfs_inject.py "$IMAGE" build/wlclient wlclient
        WLREAL=" linuxrun /sfs/linux-wlreal linuxrun /sfs/linux-wlinfo linuxrun /sfs/layer-hit linuxrun /sfs/drmtest linuxrun /sfs/rendertest"
    else
        WLREAL=""
        echo "note: third_party/wayland-aarch64 is missing; skipped the real libwayland test" >&2
    fi
    python3 scripts/sfs_inject.py "$IMAGE" build/dyn-hello linux-dyn
    python3 scripts/sfs_inject.py "$IMAGE" build/dyn-signal linux-dynsig
    python3 scripts/sfs_inject.py "$IMAGE" build/pthread-test linux-pthread
    python3 scripts/sfs_inject.py "$IMAGE" build/pthread-exit linux-pthread-exit
    python3 scripts/sfs_inject.py "$IMAGE" build/group-signal linux-groupsig
    python3 scripts/sfs_inject.py "$IMAGE" build/exitgroup-thread linux-exitgroup
    python3 scripts/sfs_inject.py "$IMAGE" build/sockets linux-sockets
    python3 scripts/sfs_inject.py "$IMAGE" build/memfd-shared linux-memfd
    python3 scripts/sfs_inject.py "$IMAGE" build/threadfork linux-threadfork
    python3 scripts/sfs_inject.py "$IMAGE" build/futexping linux-futexping
    python3 scripts/sfs_inject.py "$IMAGE" build/scratch linux-scratch
    python3 scripts/sfs_inject.py "$IMAGE" build/eventloop linux-eventloop
    DYN=" linuxrun /sfs/linux-dyn linuxrun /sfs/linux-dynsig linuxrun /sfs/linux-pthread linuxrun /sfs/linux-pthread-exit linuxrun /sfs/linux-groupsig linuxrun /sfs/linux-exitgroup linuxrun /sfs/linux-sockets linuxrun /sfs/linux-memfd linuxrun /sfs/linux-threadfork linuxrun /sfs/linux-futexping linuxrun /sfs/linux-scratch linuxrun /sfs/linux-eventloop$WLREAL"
else
    DYN=""
    echo "note: patchelf or the dynamic AArch64 libc is missing; skipped the dynamic test" >&2
fi

python3 scripts/sfs_fsck.py "$IMAGE"
echo "installed: linuxrun /sfs/linux-hello  linuxrun /sfs/linux-rw$CLOSE_RANGE$LIBC$DYN"
