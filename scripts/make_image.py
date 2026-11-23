#!/usr/bin/env python3
"""Turn the kernel ELF into a bootable raw AArch64 Image.

Why this exists: an ELF is convenient under QEMU `-kernel`, but a board
bootloader (and QEMU's own Linux-Image boot protocol) wants a flat image plus a
`text_offset` that says where it must be loaded. That path is also the only one
where a real device tree reaches the kernel, which is what the tree-first
console and driver binding are verified against.

What it does, and nothing more:

  1. `objcopy -O binary` the linked kernel;
  2. read `_head` from the ELF to learn the link address;
  3. write `text_offset = link_pa - DRAM base` into the 64-byte Image header;
  4. check the header magic and that `image_size` fits the payload.

Usage:
    scripts/make_image.py                      # release ELF -> dist/silver.img
    scripts/make_image.py --elf ... --out ...
    scripts/make_image.py --dram-base 0x0 --text-offset 0x80000

The `--dram-base` default is QEMU `virt` (0x4000_0000). A board that loads at
physical 0 passes `--dram-base 0x0`; a board whose firmware ignores
`text_offset` can pass `--text-offset 0` to keep the historical value.
"""

from __future__ import annotations

import argparse
import shutil
import struct
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_ELF = REPO / "target/aarch64-unknown-none/release/silver_microkernel"
DEFAULT_OUT = REPO / "dist/silver.img"

IMAGE_MAGIC = 0x644D5241  # "ARM\x64" little-endian in memory
MAGIC_OFFSET = 0x38
TEXT_OFFSET_OFFSET = 0x08
IMAGE_SIZE_OFFSET = 0x10
HEADER_SIZE = 0x40


def objcopy() -> str:
    for candidate in (
        "llvm-objcopy",
        "aarch64-linux-gnu-objcopy",
        "aarch64-none-elf-objcopy",
    ):
        found = shutil.which(candidate)
        if found:
            return found
    raise SystemExit("no objcopy found (tried llvm-objcopy, aarch64-linux-gnu-objcopy)")


def symbol_address(elf: Path, symbol: str) -> int:
    """Returns the virtual address of `symbol`, via readelf."""
    for tool in ("llvm-readelf", "aarch64-linux-gnu-readelf", "readelf"):
        path = shutil.which(tool)
        if not path:
            continue
        out = subprocess.run(
            [path, "--syms", str(elf)], check=True, capture_output=True, text=True
        ).stdout
        for line in out.splitlines():
            fields = line.split()
            # NUM: VALUE SIZE TYPE BIND VIS DEFAULT SECTION NAME
            if len(fields) >= 8 and fields[-1] == symbol:
                return int(fields[1], 16)
    raise SystemExit(f"symbol {symbol} not found in {elf}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--elf", type=Path, default=DEFAULT_ELF)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--dram-base", type=lambda v: int(v, 0), default=None,
                        help="DRAM base; default: the ELF's __dram_base symbol")
    parser.add_argument("--text-offset", type=lambda v: int(v, 0), default=None,
                        help="override the computed load offset (default: link - dram base)")
    args = parser.parse_args()

    if not args.elf.is_file():
        raise SystemExit(f"kernel ELF not found: {args.elf} (build it first)")
    args.out.parent.mkdir(parents=True, exist_ok=True)

    subprocess.run([objcopy(), "-O", "binary", str(args.elf), str(args.out)], check=True)
    image = bytearray(args.out.read_bytes())

    # `_head` is a high-half virtual address; `__kernel_link_pa` is the
    # physical address the image is linked for (an ABS symbol in the stub).
    try:
        link_pa = symbol_address(args.elf, "__kernel_link_pa")
    except SystemExit:
        link_pa = symbol_address(args.elf, "_head")

    # The DRAM base comes from the same build that set the link address, so a
    # board build (`SILVER_DRAM_BASE=...`) needs no extra flag here. The
    # override exists for an image built by other means.
    dram_base = args.dram_base
    if dram_base is None:
        try:
            dram_base = symbol_address(args.elf, "__dram_base")
        except SystemExit:
            dram_base = 0x4000_0000
    text_offset = args.text_offset
    if text_offset is None:
        text_offset = link_pa - dram_base
        if text_offset < 0:
            raise SystemExit(
                f"link address 0x{link_pa:x} is below the DRAM base 0x{dram_base:x}; "
                "pass --dram-base explicitly if the build's __dram_base is wrong"
            )
    if len(image) < HEADER_SIZE + 0x40:
        raise SystemExit("payload is too small to contain an AArch64 Image header")

    magic = struct.unpack_from("<I", image, MAGIC_OFFSET)[0]
    if magic != IMAGE_MAGIC:
        raise SystemExit(
            f"no AArch64 Image header (magic 0x{magic:08x} at 0x{MAGIC_OFFSET:x}); "
            "the linker script must place the header first"
        )
    # `image_size` counts the memory image, including .bss, so it is normally
    # larger than the file payload. Only a *smaller* value is a contradiction.
    image_size_field = struct.unpack_from("<Q", image, IMAGE_SIZE_OFFSET)[0]
    if image_size_field != 0 and image_size_field < len(image):
        raise SystemExit(
            f"header claims {image_size_field} bytes but the payload is {len(image)}"
        )

    struct.pack_into("<Q", image, TEXT_OFFSET_OFFSET, text_offset)
    args.out.write_bytes(bytes(image))

    load_pa = dram_base + text_offset
    print(f"{args.out}  {len(image)} bytes ({image_size_field} with .bss)")
    print(f"  link address 0x{link_pa:x}  text_offset 0x{text_offset:x}  load at 0x{load_pa:x}")
    print("  QEMU check (delivers a device tree, so the tree-first console is used):")
    print(f"    qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 2 -m 1024M \\")
    print(f"      -display none -dtb <tree.dtb> -kernel {args.out} \\")
    print("      -drive file=disk.img,format=raw,if=none,id=hd -device virtio-blk-pci,drive=hd \\")
    print("      -serial mon:stdio")
    return 0


if __name__ == "__main__":
    sys.exit(main())
