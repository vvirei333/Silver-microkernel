#!/usr/bin/env python3
"""Formats an empty SFS image: superblock in block 0, free-block bitmap in
block 1, an inode table starting at block 2, data blocks after it. The layout
matches the kernel's `sfs_format` module (and the historical format_sfs.py).

Usage: make_sfs.py [image] [megabytes] [inode_count]
Defaults: disk.img, 16 MiB, 512 inodes.
"""
import struct, sys

BLOCK = 512
INODE_SIZE = 64
MAGIC = 0x53465321

def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "disk.img"
    mib = int(sys.argv[2]) if len(sys.argv) > 2 else 16
    inode_count = int(sys.argv[3]) if len(sys.argv) > 3 else 512

    total_blocks = mib * 1024 * 1024 // BLOCK
    # The bitmap is ceil(total_blocks / 8) bytes and may span several blocks;
    # the inode table must start after the whole bitmap, never overlap it.
    bitmap_blocks = (total_blocks + 7) // 8 // BLOCK + 1
    inode_table_blocks = (inode_count * INODE_SIZE + BLOCK - 1) // BLOCK
    data_start = 1 + bitmap_blocks + inode_table_blocks

    image = bytearray(total_blocks * BLOCK)
    struct.pack_into("<I", image, 0, MAGIC)                 # magic
    struct.pack_into("<I", image, 8, BLOCK)                 # block_size
    struct.pack_into("<I", image, 12, total_blocks)         # total_blocks
    struct.pack_into("<I", image, 16, 0)                    # root_inode
    struct.pack_into("<I", image, 20, inode_count)          # inode_count
    struct.pack_into("<I", image, 24, 1)                    # bitmap_block
    struct.pack_into("<I", image, 28, 1 + bitmap_blocks)    # inode_table_block
    struct.pack_into("<I", image, 36, data_start)           # data_block_start

    # Root directory inode (kind DIR, empty block list), first slot of the
    # inode table (which starts after the whole bitmap).
    root_off = (1 + bitmap_blocks) * BLOCK
    struct.pack_into("<I", image, root_off, 2)              # INODE_KIND_DIR

    # Bitmap: mark metadata blocks used (superblock, bitmap, inode table) so
    # the allocator never hands them out. Bits for data blocks stay clear.
    bitmap_off = BLOCK
    for block in range(data_start):
        image[bitmap_off + block // 8] |= 1 << (block % 8)

    with open(path, "wb") as fh:
        fh.write(image)
    print(f"{path}: {mib} MiB, {total_blocks} blocks, {inode_count} inodes, "
          f"data from block {data_start}")

if __name__ == "__main__":
    main()
