#!/usr/bin/env python3
"""Adds or replaces one file in an existing SFS image.

The on-disk format is the one `scripts/make_sfs.py` creates: 512-byte blocks,
a free-block bitmap in block 1, a 64-byte inode table, and 32-byte directory
entries (u32 inode, u8 name length, name bytes).

This is the offline installer used by the Linux ABI track: it puts a real
foreign-toolchain ELF into `disk.img` so the guest can `run` it from `/sfs`
without embedding the binary in the kernel image.

Usage:
  python3 scripts/sfs_inject.py disk.img /path/to/file.elf [name]

`name` defaults to the source basename. The destination is the image root
directory (mounted as `/sfs`). An existing entry with the same name is
replaced, freeing the old blocks first. Files up to the VFS limit (4 direct +
128 indirect + 128*128 double-indirect + 128^3 triple-indirect blocks) are
supported.
"""

import mmap
import os
import struct
import sys

BLOCK = 512
INODE_SIZE = 64
DIRECT = 4
INDIRECT_POINTERS = BLOCK // 4
INDIRECT_SQUARED = INDIRECT_POINTERS * INDIRECT_POINTERS
DOUBLE_FIRST = DIRECT + INDIRECT_POINTERS
TRIPLE_FIRST = DOUBLE_FIRST + INDIRECT_SQUARED
INODE_KIND_FREE = 0
INODE_KIND_FILE = 1
INODE_KIND_DIR = 2
NAME_MAX = 27


class Image:
    def __init__(self, path: str):
        self.path = path
        # mmap instead of reading the whole image: the Phosh image is 2 GiB
        # and copying it into a bytearray per injection exhausted host RAM.
        self._handle = open(path, "r+b")
        self.data = mmap.mmap(self._handle.fileno(), 0)
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
        if self.magic != 0x53465321:
            raise SystemExit("not an SFS image (bad magic)")
        if self.block_size != BLOCK:
            raise SystemExit(f"unsupported block size {self.block_size}")
        self.next_block = self.data_start

    # --- geometry helpers -------------------------------------------------
    def block_offset(self, block: int) -> int:
        if not (self.data_start <= block < self.total):
            raise SystemExit(f"block {block} outside the data area")
        return block * BLOCK

    def used(self, block: int) -> bool:
        return bool(self.data[self.bitmap * BLOCK + block // 8] >> (block % 8) & 1)

    def set_used(self, block: int, used: bool = True) -> None:
        offset = self.bitmap * BLOCK + block // 8
        mask = 1 << (block % 8)
        if used:
            self.data[offset] |= mask
        else:
            self.data[offset] &= ~mask & 0xFF
            if block < self.next_block:
                self.next_block = block

    def alloc_block(self) -> int:
        # A cursor keeps bulk installs linear: rescanning the whole bitmap for
        # every 512-byte block made a 75 MiB library set take tens of minutes.
        for block in range(self.next_block, self.total):
            if not self.used(block):
                self.set_used(block)
                self.next_block = block + 1
                return block
        for block in range(self.data_start, self.next_block):
            if not self.used(block):
                self.set_used(block)
                return block
        raise SystemExit("image is full")

    # --- inodes -----------------------------------------------------------
    def inode_offset(self, index: int) -> int:
        if not (0 <= index < self.inodes):
            raise SystemExit(f"inode {index} out of range")
        return self.itable * BLOCK + index * INODE_SIZE

    def inode(self, index: int):
        off = self.inode_offset(index)
        kind, size = struct.unpack_from("<II", self.data, off)
        blocks = list(struct.unpack_from("<4I", self.data, off + 8))
        indirect = struct.unpack_from("<I", self.data, off + 24)[0]
        double = struct.unpack_from("<I", self.data, off + 28)[0]
        triple = struct.unpack_from("<I", self.data, off + 32)[0]
        return kind, size, blocks, indirect, double, triple

    def set_inode(self, index, kind, size, blocks, indirect, double, triple=0):
        off = self.inode_offset(index)
        struct.pack_into("<II", self.data, off, kind, size)
        packed = (blocks + [0, 0, 0, 0])[:4]
        struct.pack_into("<4I", self.data, off + 8, *packed)
        struct.pack_into("<III", self.data, off + 24, indirect, double, triple)

    def alloc_inode(self, kind: int) -> int:
        for index in range(self.inodes):
            if self.inode(index)[0] == INODE_KIND_FREE:
                self.set_inode(index, kind, 0, [0, 0, 0, 0], 0, 0, 0)
                return index
        raise SystemExit("inode table is full")

    # --- directory entries ------------------------------------------------
    def dir_blocks(self, dir_index: int):
        _, _, blocks, indirect, double, triple = self.inode(dir_index)
        return self.block_list(blocks, indirect, double, triple)

    def block_list(self, blocks, indirect, double, triple):
        result = [block for block in blocks if block]
        if indirect:
            table = struct.unpack_from("<128I", self.data, indirect * BLOCK)
            result.extend(block for block in table if block)
        if double:
            node = struct.unpack_from("<128I", self.data, double * BLOCK)
            for table_block in node:
                if not table_block:
                    continue
                table = struct.unpack_from("<128I", self.data, table_block * BLOCK)
                result.extend(block for block in table if block)
        if triple:
            root = struct.unpack_from("<128I", self.data, triple * BLOCK)
            for double_block in root:
                if not double_block:
                    continue
                node = struct.unpack_from("<128I", self.data, double_block * BLOCK)
                for table_block in node:
                    if not table_block:
                        continue
                    table = struct.unpack_from("<128I", self.data, table_block * BLOCK)
                    result.extend(block for block in table if block)
        return result

    def find_entry(self, dir_index: int, name: str):
        encoded = name.encode()
        for block in self.dir_blocks(dir_index):
            start = block * BLOCK
            for slot in range(BLOCK // 32):
                entry = start + slot * 32
                inode = struct.unpack_from("<I", self.data, entry)[0]
                if inode == 0:
                    continue
                length = self.data[entry + 4]
                if length == len(encoded) and self.data[entry + 5 : entry + 5 + length] == encoded:
                    return inode
        return None

    def add_entry(self, dir_index: int, name: str, inode: int) -> None:
        encoded = name.encode()
        if not encoded or len(encoded) > NAME_MAX:
            raise SystemExit(f"name {name!r} must be 1..{NAME_MAX} bytes")
        # Reuse a free slot in an existing directory block first.
        for block in self.dir_blocks(dir_index):
            start = block * BLOCK
            for slot in range(BLOCK // 32):
                entry = start + slot * 32
                if struct.unpack_from("<I", self.data, entry)[0] == 0:
                    self.write_entry(entry, inode, encoded)
                    return
        # No slot: grow the directory by one block.
        _, _, blocks, indirect, double, triple = self.inode(dir_index)
        block = self.alloc_block()
        for byte in range(BLOCK):
            self.data[block * BLOCK + byte] = 0
        blocks, indirect, double, triple = self.append_block(
            blocks, indirect, double, triple, block, len(self.dir_blocks(dir_index))
        )
        self.set_inode(dir_index, INODE_KIND_DIR, 0, blocks, indirect, double, triple)
        self.write_entry(block * BLOCK, inode, encoded)

    def write_entry(self, offset: int, inode: int, encoded: bytes) -> None:
        struct.pack_into("<I", self.data, offset, inode)
        self.data[offset + 4] = len(encoded)
        self.data[offset + 5 : offset + 5 + len(encoded)] = encoded
        for byte in range(offset + 5 + len(encoded), offset + 32):
            self.data[byte] = 0

    # --- block lists ------------------------------------------------------
    def append_block(self, blocks, indirect, double, triple, block, slot_index):
        """Places `block` at logical slot `slot_index` (0-based)."""
        if slot_index < DIRECT:
            blocks[slot_index] = block
            return blocks, indirect, double, triple
        if slot_index < DOUBLE_FIRST:
            if not indirect:
                indirect = self.new_table()
            pointer = slot_index - DIRECT
            struct.pack_into("<I", self.data, indirect * BLOCK + pointer * 4, block)
            return blocks, indirect, double, triple
        if slot_index < TRIPLE_FIRST:
            position = slot_index - DOUBLE_FIRST
            table_index, pointer = divmod(position, INDIRECT_POINTERS)
            if not double:
                double = self.new_table()
            table = struct.unpack_from("<I", self.data, double * BLOCK + table_index * 4)[0]
            if not table:
                table = self.new_table()
                struct.pack_into("<I", self.data, double * BLOCK + table_index * 4, table)
            struct.pack_into("<I", self.data, table * BLOCK + pointer * 4, block)
            return blocks, indirect, double, triple
        position = slot_index - TRIPLE_FIRST
        outer, remainder = divmod(position, INDIRECT_SQUARED)
        middle, pointer = divmod(remainder, INDIRECT_POINTERS)
        if not triple:
            triple = self.new_table()
        double_block = struct.unpack_from("<I", self.data, triple * BLOCK + outer * 4)[0]
        if not double_block:
            double_block = self.new_table()
            struct.pack_into("<I", self.data, triple * BLOCK + outer * 4, double_block)
        table = struct.unpack_from("<I", self.data, double_block * BLOCK + middle * 4)[0]
        if not table:
            table = self.new_table()
            struct.pack_into("<I", self.data, double_block * BLOCK + middle * 4, table)
        struct.pack_into("<I", self.data, table * BLOCK + pointer * 4, block)
        return blocks, indirect, double, triple

    def new_table(self) -> int:
        block = self.alloc_block()
        for byte in range(BLOCK):
            self.data[block * BLOCK + byte] = 0
        return block

    def free_file(self, index: int) -> None:
        kind, _, blocks, indirect, double, triple = self.inode(index)
        if kind != INODE_KIND_FILE:
            raise SystemExit(f"inode {index} is not a file")
        for block in self.block_list(blocks, indirect, double, triple):
            self.set_used(block, False)
        if indirect:
            self.set_used(indirect, False)
        if double:
            node = struct.unpack_from("<128I", self.data, double * BLOCK)
            for table in node:
                if table:
                    self.set_used(table, False)
            self.set_used(double, False)
        if triple:
            root = struct.unpack_from("<128I", self.data, triple * BLOCK)
            for double_block in root:
                if not double_block:
                    continue
                node = struct.unpack_from("<128I", self.data, double_block * BLOCK)
                for table in node:
                    if table:
                        self.set_used(table, False)
                self.set_used(double_block, False)
            self.set_used(triple, False)
        self.set_inode(index, INODE_KIND_FREE, 0, [0, 0, 0, 0], 0, 0, 0)

    def mkdir(self, path: str) -> int:
        """Creates `path` (relative to the root) and returns its inode."""
        index = self.root
        for part in (piece for piece in path.split("/") if piece):
            child = self.find_entry(index, part)
            if child is None:
                child = self.alloc_inode(INODE_KIND_DIR)
                block = self.alloc_block()
                for byte in range(BLOCK):
                    self.data[block * BLOCK + byte] = 0
                self.set_inode(child, INODE_KIND_DIR, 0, [block, 0, 0, 0], 0, 0, 0)
                self.add_entry(index, part, child)
            index = child
        return index

    def write_file(self, name: str, payload: bytes) -> None:
        self.write_file_in(self.root, name, payload)

    def write_file_at(self, path: str, payload: bytes) -> None:
        """Writes a file at a slash-separated path, creating parents."""
        parts = [piece for piece in path.split("/") if piece]
        if not parts:
            raise SystemExit("empty destination path")
        parent = self.mkdir("/".join(parts[:-1])) if len(parts) > 1 else self.root
        self.write_file_in(parent, parts[-1], payload)

    def write_file_in(self, dir_index: int, name: str, payload: bytes) -> None:
        existing = self.find_entry(dir_index, name)
        if existing is not None:
            self.free_file(existing)
            index = existing
        else:
            index = self.alloc_inode(INODE_KIND_FILE)
        blocks = [0, 0, 0, 0]
        indirect = 0
        double = 0
        triple = 0
        count = (len(payload) + BLOCK - 1) // BLOCK
        for slot in range(count):
            block = self.alloc_block()
            chunk = payload[slot * BLOCK : (slot + 1) * BLOCK]
            start = block * BLOCK
            self.data[start : start + len(chunk)] = chunk
            for byte in range(start + len(chunk), start + BLOCK):
                self.data[byte] = 0
            blocks, indirect, double, triple = self.append_block(
                blocks, indirect, double, triple, block, slot
            )
        self.set_inode(
            index, INODE_KIND_FILE, len(payload), blocks, indirect, double, triple
        )
        if existing is None:
            self.add_entry(dir_index, name, index)

    def save(self) -> None:
        self.data.flush()


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    image_path, source = sys.argv[1], sys.argv[2]
    name = sys.argv[3] if len(sys.argv) > 3 else os.path.basename(source)
    with open(source, "rb") as handle:
        payload = handle.read()
    image = Image(image_path)
    image.write_file(name, payload)
    image.save()
    print(f"{image_path}: wrote {len(payload)} bytes as /{name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
