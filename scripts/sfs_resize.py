#!/usr/bin/env python3
"""Grows an existing SFS image in place, preserving every file and directory.

SFS stores free-block bits and the inode table at fixed early blocks, so a
larger image needs a different metadata geometry (a bigger bitmap and usually
a bigger inode table). Rather than moving raw blocks around and rewriting every
inode reference, this tool builds a new image and copies the filesystem tree:

  * inode numbers and directory entries stay identical (so paths never change);
  * every data block and indirect table (single, double or triple) is
    reallocated sequentially in the new image and written back into the inode
    that owned it.

Usage:
  sfs_resize.py [image] [megabytes] [inode_count]

Defaults: disk.img, 64 MiB, 2048 inodes. A smaller target than the current image
(or fewer inodes than are in use) is rejected; running the tool on an image
that already has the requested geometry is a no-op.
"""

import mmap
import struct
import sys

BLOCK = 512
INODE_SIZE = 64
MAGIC = 0x53465321
DIRECT = 4
INDIRECT_POINTERS = BLOCK // 4
INODE_KIND_FREE = 0


def bitmap_blocks(total_blocks: int) -> int:
    # Keep the same conservative geometry as scripts/make_sfs.py: one extra
    # block of headroom after the bitmap.
    return (total_blocks + 7) // 8 // BLOCK + 1


def inode_table_blocks(inode_count: int) -> int:
    return (inode_count * INODE_SIZE + BLOCK - 1) // BLOCK


def data_start_for(total_blocks: int, inode_count: int) -> int:
    return 1 + bitmap_blocks(total_blocks) + inode_table_blocks(inode_count)


class Image:
    def __init__(self, path: str):
        self.path = path
        with open(path, "rb") as handle:
            self.data = bytearray(handle.read())
        if len(self.data) < BLOCK:
            raise SystemExit(f"{path}: too small to be an SFS image")
        fields = struct.unpack_from("<10I", self.data, 0)
        (
            self.magic,
            _version,
            self.block_size,
            self.total,
            self.root,
            self.inodes,
            self.bitmap,
            self.itable,
            _next,
            self.data_start,
        ) = fields
        if self.magic != MAGIC:
            raise SystemExit(f"{path}: not an SFS image (bad magic)")
        if self.block_size != BLOCK:
            raise SystemExit(f"{path}: unsupported block size {self.block_size}")
        if len(self.data) < self.total * BLOCK:
            raise SystemExit(f"{path}: truncated image")

    def inode_offset(self, index: int) -> int:
        return self.itable * BLOCK + index * INODE_SIZE

    def inode(self, index: int):
        offset = self.inode_offset(index)
        kind, size = struct.unpack_from("<II", self.data, offset)
        blocks = list(struct.unpack_from("<4I", self.data, offset + 8))
        indirect, double, triple = struct.unpack_from("<III", self.data, offset + 24)
        return kind, size, blocks, indirect, double, triple

    def block_bytes(self, block: int) -> bytes:
        start = block * BLOCK
        return bytes(self.data[start : start + BLOCK])

    def pointers(self, block: int):
        return list(struct.unpack_from("<128I", self.data, block * BLOCK))


class Builder:
    """New image under construction; allocates blocks from the data area."""

    def __init__(self, old: Image, total_blocks: int, inode_count: int,
                 path: str):
        self.old = old
        self.total = total_blocks
        self.inodes = inode_count
        self.bitmap_blocks = bitmap_blocks(total_blocks)
        self.itable = 1 + self.bitmap_blocks
        self.data_start = data_start_for(total_blocks, inode_count)
        # Sparse destination: truncate to the requested size and mmap it.
        # The untouched tail stays holes on disk and costs no host RAM.
        self._handle = open(path, "wb")
        self._handle.truncate(total_blocks * BLOCK)
        self._handle.close()
        self._handle = open(path, "r+b")
        self.data = mmap.mmap(self._handle.fileno(), 0)
        self.next_block = self.data_start

    def close(self) -> None:
        self.data.flush()
        self.data.close()
        self._handle.close()

    def alloc(self) -> int:
        if self.next_block >= self.total:
            raise SystemExit("target image is too small for the existing files")
        block = self.next_block
        self.next_block += 1
        # The bitmap lives in block 1; mark the block as soon as it is handed
        # out so a partially copied tree never claims free space.
        self.data[BLOCK + block // 8] |= 1 << (block % 8)
        return block

    def copy_block(self, source: int) -> int:
        block = self.alloc()
        start = block * BLOCK
        self.data[start : start + BLOCK] = self.old.block_bytes(source)
        return block

    def copy_tree(self, blocks, indirect: int, double: int, triple: int):
        """Reallocates one inode's block tree and returns the new pointers."""
        new_blocks = [0, 0, 0, 0]
        for slot, block in enumerate(blocks[:4]):
            if block:
                new_blocks[slot] = self.copy_block(block)

        new_indirect = 0
        if indirect:
            new_indirect = self.alloc()
            for slot, block in enumerate(self.old.pointers(indirect)):
                if block:
                    struct.pack_into(
                        "<I", self.data, new_indirect * BLOCK + slot * 4,
                        self.copy_block(block),
                    )

        new_double = 0
        if double:
            new_double = self.alloc()
            for slot, table in enumerate(self.old.pointers(double)):
                if not table:
                    continue
                new_table = self.alloc()
                struct.pack_into(
                    "<I", self.data, new_double * BLOCK + slot * 4, new_table
                )
                for entry, block in enumerate(self.old.pointers(table)):
                    if block:
                        struct.pack_into(
                            "<I", self.data,
                            new_table * BLOCK + entry * 4,
                            self.copy_block(block),
                        )

        new_triple = 0
        if triple:
            new_triple = self.alloc()
            for outer, double_block in enumerate(self.old.pointers(triple)):
                if not double_block:
                    continue
                new_sub_double = self.alloc()
                struct.pack_into(
                    "<I", self.data, new_triple * BLOCK + outer * 4, new_sub_double
                )
                for middle, table in enumerate(self.old.pointers(double_block)):
                    if not table:
                        continue
                    new_table = self.alloc()
                    struct.pack_into(
                        "<I", self.data, new_sub_double * BLOCK + middle * 4, new_table
                    )
                    for entry, block in enumerate(self.old.pointers(table)):
                        if block:
                            struct.pack_into(
                                "<I", self.data,
                                new_table * BLOCK + entry * 4,
                                self.copy_block(block),
                            )

        return new_blocks, new_indirect, new_double, new_triple

    def copy_inodes(self) -> None:
        for index in range(self.old.inodes):
            kind, size, blocks, indirect, double, triple = self.old.inode(index)
            if kind == INODE_KIND_FREE:
                continue
            new_blocks, new_indirect, new_double, new_triple = self.copy_tree(
                blocks, indirect, double, triple
            )
            destination = self.itable * BLOCK + index * INODE_SIZE
            struct.pack_into("<II", self.data, destination, kind, size)
            struct.pack_into("<4I", self.data, destination + 8, *new_blocks)
            struct.pack_into(
                "<III", self.data, destination + 24,
                new_indirect, new_double, new_triple,
            )

    def finish(self, root: int) -> bytes:
        struct.pack_into("<I", self.data, 0, MAGIC)
        struct.pack_into("<I", self.data, 8, BLOCK)
        struct.pack_into("<I", self.data, 12, self.total)
        struct.pack_into("<I", self.data, 16, root)
        struct.pack_into("<I", self.data, 20, self.inodes)
        struct.pack_into("<I", self.data, 24, 1)             # bitmap block
        struct.pack_into("<I", self.data, 28, self.itable)   # inode table block
        struct.pack_into("<I", self.data, 36, self.data_start)

        # Mark superblock, bitmap and inode table with one pass: every block
        # below the data area belongs to metadata.
        for block in range(self.data_start):
            self.data[BLOCK + block // 8] |= 1 << (block % 8)


def main(argv) -> int:
    path = argv[1] if len(argv) > 1 else "disk.img"
    megabytes = int(argv[2]) if len(argv) > 2 else 16
    inode_count = int(argv[3]) if len(argv) > 3 else 512

    if megabytes <= 0 or inode_count <= 0:
        raise SystemExit("megabytes and inode_count must be positive")

    current = Image(path)
    total_blocks = megabytes * 1024 * 1024 // BLOCK
    if total_blocks < current.total:
        raise SystemExit(
            f"{path}: refusing to shrink {current.total} blocks to {total_blocks}"
        )

    used_inodes = max(
        (index + 1 for index in range(current.inodes) if current.inode(index)[0] != 0),
        default=1,
    )
    if inode_count < used_inodes:
        raise SystemExit(
            f"{path}: {used_inodes} inodes are in use, cannot shrink to {inode_count}"
        )

    if total_blocks == current.total and inode_count == current.inodes:
        print(
            f"{path}: already {megabytes} MiB, {current.inodes} inodes "
            f"(data from block {current.data_start})"
        )
        return 0

    builder = Builder(current, total_blocks, inode_count, path)
    builder.copy_inodes()
    builder.finish(current.root)
    builder.close()
    print(
        f"{path}: resized {current.total} -> {total_blocks} blocks "
        f"({megabytes} MiB), {current.inodes} -> {inode_count} inodes, "
        f"data from block {builder.data_start}; "
        f"{builder.next_block - builder.data_start} blocks copied"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
