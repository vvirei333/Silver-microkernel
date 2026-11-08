# WerewolfOS window viewer

The guest browser draws one RGBA surface into an ivshmem BAR. This Qt
program maps the same file on the host, shows the frames and sends keys and
wheel events back through the shared input ring.

## Build

```bash
cmake -S tools/window-viewer -B build/window-viewer
cmake --build build/window-viewer
```

## KDE / GNOME, Wayland / X11

The viewer is a normal Qt6 Widgets program, so it is a native window under
both desktops. Qt selects Wayland when `WAYLAND_DISPLAY` is set and X11
otherwise; set `QT_QPA_PLATFORM=wayland` or `xcb` to force one. The guest
draws, the host blits and forwards input, so no compositor or GPU is involved.

The viewer also links **Qt6 Network** and acts as Wolfser's TLS proxy: when the
guest publishes an `https://` URL in the shared header, the viewer fetches it
with Qt's TLS stack and writes the body back (respecting the body cap). A
viewer without Network support leaves `FIELD_WEB_CAPABLE` at 0, and the guest
reports that HTTPS needs a rebuilt viewer.

## Run

Start QEMU with a shared memory file and the ivshmem device (or just run
`./build_and_qemu.sh --viewer`, which does this and launches the viewer):

```bash
qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 1 -m 256M \
  -kernel target/aarch64-unknown-none/release/silver_microkernel \
  -drive file=disk.img,format=raw,if=none,id=hd \
  -device virtio-blk-pci,drive=hd \
  -device virtio-net-pci,disable-legacy=on \
  -object memory-backend-file,id=ivshmem0,share=on,mem-path=/dev/shm/werewolf-window,size=4M \
  -device ivshmem-plain,memdev=ivshmem0 \
  -display none -serial mon:stdio

# in the guest
howl> wolfser            # search bar
howl> wolfser example.com
howl> wolfser build a bear
```

Then, on the host:

```bash
tools/wolfser-host.sh           # builds if needed, then opens the window
# or: build/window-viewer/werewolf-window --shm /dev/shm/werewolf-window
# hidden until the guest asks:  ... --watch --shm /dev/shm/werewolf-window
```

With `--watch` the viewer starts hidden, waits for the guest's window request
(the `wolfser` command) and only then shows the window; nothing appears while
the kernel boots. Without `--watch` it opens at once with a "Waiting for
Wolfser" screen. Neither mode times out while waiting.

Keys: printable text goes to the search bar, `Enter` navigates, `Backspace`
edits, `Ctrl+L` starts a new address, `r` reloads, `q` quits, and
arrows/PageUp/PageDown/Home/End plus the wheel scroll.

## Headless check

```bash
QT_QPA_PLATFORM=offscreen \
  build/window-viewer/werewolf-window --shm /dev/shm/werewolf-window \
  --snapshot /tmp/browser.png --timeout 20
```

It prints `snapshot: WxH frame=N nonwhite-samples=K` and exits 0 once a frame
has been published.
