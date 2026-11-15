#!/usr/bin/env python3
"""Reproduce the WerewolfOS 0.1.2 demo and record it.

The script boots the release kernel with the Phosh image, runs the console
scenario on the serial line (fastfetch, then `linuxstart phosh`), drives the
GUI through the window input ring and records the guest's own scanout from the
ivshmem window (`/dev/shm/...`): the frames are the pixels the kernel
published, not a screen capture of the host desktop.

Requirements: qemu-system-aarch64, ffmpeg (optional, for the MP4/GIF) and
Pillow.  The image defaults to $PHOSH_IMAGE or ~/phosh-state/disk_phosh.img.

Usage:
    tools/demo.py                 # boot, drive, record, encode
    tools/demo.py --no-record     # only drive the scenario
    tools/demo.py --keep-frames   # keep the PNG stills next to the video
"""
import argparse
import mmap
import os
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SHM = "/dev/shm/werewolf-demo"
SERIAL = "/tmp/werewolf-demo.serial"
MONITOR = "/tmp/werewolf-demo.monitor"
FPS = 10

# window_proto.rs: non-printable keys use these small values.
KEY_ENTER = 10


class Session:
    def __init__(self, image, record, frames_dir=None):
        self.image = image
        self.record = record
        self.frames_dir = frames_dir
        self.proc = None
        self.sock = None
        self.serial = bytearray()
        self.serial_log = Path("/tmp/werewolf-demo.log")
        self.log_handle = None
        self.frame_count = 0
        self.recording = False
        self.thread = None

    # --- boot -----------------------------------------------------------
    def start(self):
        kernel = ROOT / "target/aarch64-unknown-none/release/silver_microkernel"
        if not kernel.exists():
            sys.exit("build first: ./build_and_qemu.sh release --no-run")
        for path in (SERIAL, MONITOR):
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
        if os.path.exists(SHM):
            os.unlink(SHM)
        self.proc = subprocess.Popen([
            "qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a72",
            "-smp", "2", "-m", "1024M",
            "-kernel", str(kernel),
            "-drive", f"file={self.image},format=raw,if=none,id=hd",
            "-device", "virtio-blk-pci,drive=hd",
            "-device", "virtio-net-pci,disable-legacy=on",
            "-object", f"memory-backend-file,id=ivshmem0,share=on,"
                       f"mem-path={SHM},size=4M",
            "-device", "ivshmem-plain,memdev=ivshmem0",
            "-display", "none",
            "-serial", f"unix:{SERIAL},server=on,wait=off",
            "-monitor", f"unix:{MONITOR},server=on,wait=off",
        ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(300):
            try:
                sock = socket.socket(socket.AF_UNIX)
                sock.connect(SERIAL)
                self.sock = sock
                break
            except OSError:
                time.sleep(0.1)
        if self.sock is None:
            sys.exit("serial socket did not appear")
        self.sock.settimeout(0.2)
        self.log_handle = open(self.serial_log, "wb", buffering=0)
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                data = self.sock.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                return
            if not data:
                return
            self.serial += data
            if self.log_handle is not None:
                self.log_handle.write(data)

    def wait_text(self, needle, timeout=120):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if needle in self.serial:
                return True
            time.sleep(0.1)
        return False

    def console(self, command):
        self.sock.sendall((command + "\n").encode())

    # --- window ---------------------------------------------------------
    def _ring(self, mem, kind, value=0, key=0, x=0, y=0):
        ring, slots = 0x100, 64
        while True:
            head = struct.unpack_from("<I", mem, 0x20)[0] % slots
            tail = struct.unpack_from("<I", mem, 0x24)[0] % slots
            if (head + slots - tail) % slots < slots - 1:
                break
            time.sleep(0.01)
        struct.pack_into("<BBHHH", mem, ring + head * 8, kind, value, key, x, y)
        struct.pack_into("<I", mem, 0x20, (head + 1) % slots)

    def input(self, *args):
        with open(SHM, "r+b") as handle:
            mem = mmap.mmap(handle.fileno(), 0)
            command = args[0]
            if command == "click":
                x, y = args[1], args[2]
                self._ring(mem, 4, 0, 0, x, y)
                time.sleep(0.05)
                self._ring(mem, 3, 1, 0, x, y)
                time.sleep(0.08)
                self._ring(mem, 5, 0, 0, x, y)
            elif command == "key":
                self._ring(mem, 1, 0, args[1])
            mem.close()
        time.sleep(0.2)

    def text(self, value, delay=0.25):
        for char in value:
            self.input("key", ord(char))
            time.sleep(delay)

    def grab(self):
        try:
            with open(SHM, "rb") as handle:
                mem = mmap.mmap(handle.fileno(), 0, prot=mmap.PROT_READ)
        except (FileNotFoundError, ValueError):
            return None
        magic, _, width, height = struct.unpack_from("<4I", mem, 0)
        stride, fmt, seq1, state = struct.unpack_from("<4I", mem, 0x10)
        if magic != 0x57494E31 or width != 1024 or height != 768 or seq1 & 1:
            mem.close()
            return None
        data = bytes(mem[0x1000:0x1000 + stride * height])
        seq2 = struct.unpack_from("<I", mem, 0x18)[0]
        mem.close()
        if seq1 != seq2:
            return None
        return Image.frombuffer("RGBA", (width, height), data, "raw", "RGBA",
                                stride, 1).convert("RGB")

    # --- recording ------------------------------------------------------
    def start_recording(self):
        self.recording = True
        self.thread = threading.Thread(target=self._capture, daemon=True)
        self.thread.start()

    def _capture(self):
        period = 1.0 / FPS
        while self.recording:
            begin = time.time()
            frame = self.grab()
            if frame is not None:
                frame.save(self.frames_dir /
                           ("frame-%05d.png" % self.frame_count))
                self.frame_count += 1
            time.sleep(max(0.0, period - (time.time() - begin)))

    def stop_recording(self):
        self.recording = False
        if self.thread:
            self.thread.join(timeout=2)

    def wait_window(self, timeout=90):
        """Wait until the terminal window covers the centre of the screen."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            frame = self.grab()
            if frame is not None:
                red, green, blue = frame.getpixel((512, 250))
                if red > 200 and green > 200 and blue > 200:
                    return True
            time.sleep(0.3)
        return False

    def stop(self):
        if self.log_handle is not None:
            self.log_handle.close()
            self.log_handle = None
        try:
            if self.proc:
                self.proc.terminate()
                self.proc.wait(timeout=5)
        except (subprocess.TimeoutExpired, ProcessLookupError):
            self.proc.kill()

    # --- scenario -------------------------------------------------------
    def run(self):
        print("boot: waiting for the Howl prompt")
        if not self.wait_text(b"howl>", 90):
            sys.exit("no howl prompt in the serial log")

        self.console("fastfetch")
        time.sleep(3)
        # The demo also prints the health summary the release checklist asks
        # for (0 zombies / 0 open files / 0 DMA leaks) before Phosh takes over
        # the console.
        self.console("health")
        time.sleep(2)
        self.console("linuxstart phosh")
        print("phosh: waiting for the shell")
        if not self.wait_text(b"ready after", 120):
            sys.exit("Phosh did not become ready")
        time.sleep(6)

        # The lock screen is a GTK widget with its own CSS background; without
        # it the compositor backdrop is black. Sample away from the clock so a
        # wallpaper regression is visible in the demo log.
        lock = self.grab()
        if lock is not None:
            red, green, blue = lock.getpixel((100, 600))
            if red + green + blue < 15:
                print("warning: lock-screen wallpaper is missing")

        if self.record:
            self.start_recording()
            print("recording %d fps" % FPS)
            time.sleep(3)

        print("unlock")
        self.input("key", 0x20)          # Space
        time.sleep(6)

        print("app grid: focus the search field")
        self.input("click", 512, 70)
        time.sleep(1)
        self.text("term")
        time.sleep(3)

        print("launch Terminal")
        # Enter activates the keyboard-focused search result; the tile click is
        # the fallback when the focus moved.
        self.input("key", KEY_ENTER)
        time.sleep(1.5)
        if self.record:
            # GTK takes ~15-20 s to come up on the Linux ABI; cut the wait out
            # of the recording instead of showing an idle app grid.
            self.stop_recording()
        if not self.wait_window(25):
            self.input("click", 72, 120)
            if not self.wait_window(60):
                sys.exit("the Terminal window did not appear")
        if self.record:
            self.start_recording()
            time.sleep(1)

        print("terminal: ls /sfs")
        self.input("click", 512, 610)
        time.sleep(1)
        self.text("ls /sfs")
        self.input("key", KEY_ENTER)
        time.sleep(5)

        if self.record:
            still = self.grab()
            if still is not None:
                still.save(ROOT / "assets/demo-0.1.2-terminal.png")
            self.stop_recording()
            time.sleep(1)

        text = bytes(self.serial).decode("utf-8", "replace")
        marker = text.rfind("WerewolfOS health")
        if marker >= 0:
            print(text[marker:marker + 180].strip())

        print("close the window")
        self.input("click", 884, 140)
        if self.record:
            time.sleep(0.5)
            self.start_recording()
            time.sleep(3)
            self.stop_recording()


def encode(frames_dir, gif, video):
    if not frames_dir.exists() or not any(frames_dir.iterdir()):
        return
    cmd = [
        "ffmpeg", "-y", "-loglevel", "error",
        "-framerate", str(FPS), "-i", str(frames_dir / "frame-%05d.png"),
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "20",
        "-movflags", "+faststart", str(video),
    ]
    subprocess.run(cmd, check=False)
    palette = [
        "ffmpeg", "-y", "-loglevel", "error", "-i", str(video),
        "-vf", "fps=8,scale=768:-1:flags=lanczos,split[s0][s1];"
               "[s0]palettegen=max_colors=192[p];[s1][p]paletteuse=dither=bayer",
        str(gif),
    ]
    subprocess.run(palette, check=False)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--image",
                        default=os.environ.get("PHOSH_IMAGE",
                                               str(Path.home() /
                                                   "phosh-state/disk_phosh.img")))
    parser.add_argument("--no-record", action="store_true")
    parser.add_argument("--keep-frames", action="store_true")
    parser.add_argument("--gif", default=str(ROOT / "assets/demo-0.1.2.gif"))
    parser.add_argument("--video", default=str(ROOT / "assets/demo-0.1.2.mp4"))
    args = parser.parse_args()

    frames_dir = Path("/tmp/werewolf-demo-frames")
    if frames_dir.exists():
        for old in frames_dir.glob("*.png"):
            old.unlink()
    frames_dir.mkdir(parents=True, exist_ok=True)
    session = Session(args.image, not args.no_record, frames_dir)

    try:
        session.start()
        session.run()
    finally:
        session.stop_recording()
        session.stop()

    if session.record:
        print("frames: %d" % session.frame_count)
        if session.frame_count:
            encode(frames_dir, Path(args.gif), Path(args.video))
            print("wrote %s and %s" % (args.gif, args.video))
        if not args.keep_frames and not session.frame_count:
            pass
        elif not args.keep_frames:
            for old in frames_dir.glob("*.png"):
                old.unlink()


if __name__ == "__main__":
    main()
