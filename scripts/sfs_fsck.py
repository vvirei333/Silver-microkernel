#!/usr/bin/env python3
"""Offline consistency check for an SFS image.

Validates the superblock, every inode's direct-block list, and the free-block
bitmap. Reports cross-links (two inodes owning one block) and bitmap drift.

Usage: python3 scripts/sfs_fsck.py [image]
Exit code 0 = clean, 1 = problems found.
"""

import struct
import sys

BLOCK = 512
INODE_SIZE = 64
DIRECT = 4
INDIRECT_POINTERS = BLOCK // 4
INDIRECT_SQUARED = INDIRECT_POINTERS * INDIRECT_POINTERS
DOUBLE_FIRST = DIRECT + INDIRECT_POINTERS
TRIPLE_FIRST = DOUBLE_FIRST + INDIRECT_SQUARED
MAX_FILE_BLOCKS = TRIPLE_FIRST + INDIRECT_SQUARED * INDIRECT_POINTERS
KIND_NAMES = {0: "free", 1: "file", 2: "dir"}


def main(path: str) -> int:
    with open(path, "rb") as f:
        image = f.read()

    if len(image) < BLOCK:
        print(f"FAIL: image is {len(image)} bytes")
        return 1
    magic, version, block_size, total, root, inodes, bitmap, itable, _nb, data_start = (
        struct.unpack_from("<10I", image, 0)
    )
    if magic != 0x53465321:
        print("FAIL: bad magic")
        return 1
    if block_size != BLOCK or total * BLOCK > len(image):
        print("FAIL: bad geometry")
        return 1
    if (
        bitmap >= total
        or itable >= total
        or data_start >= total
        or root >= inodes
        or itable * BLOCK + inodes * INODE_SIZE > len(image)
        or bitmap * BLOCK + (total + 7) // 8 > len(image)
    ):
        print("FAIL: metadata out of range")
        return 1

    def used(block: int) -> bool:
        return image[bitmap * BLOCK + block // 8] >> (block % 8) & 1 == 1

    problems = []
    owners = {}
    files = 0
    dirs = 0

    for index in range(inodes):
        off = itable * BLOCK + index * INODE_SIZE
        kind, size = struct.unpack_from("<II", image, off)
        blocks = struct.unpack_from("<4I", image, off + 8)
        indirect = struct.unpack_from("<I", image, off + 24)[0]
        double = struct.unpack_from("<I", image, off + 28)[0]
        triple = struct.unpack_from("<I", image, off + 32)[0]
        if kind == 0:
            if any(blocks) or indirect != 0 or double != 0 or triple != 0:
                problems.append(
                    f"inode {index}: free but has blocks {blocks} "
                    f"indirect {indirect} double {double} triple {triple}"
                )
            continue
        if kind == 1:
            files += 1
            if size > MAX_FILE_BLOCKS * BLOCK:
                problems.append(f"inode {index}: size {size} exceeds block-tree capacity")
        elif kind == 2:
            dirs += 1
        else:
            problems.append(f"inode {index}: unknown kind {kind}")
            continue

        for slot, block in enumerate(blocks):
            if block == 0:
                continue
            if not (data_start <= block < total):
                problems.append(f"inode {index} slot {slot}: block {block} outside data area")
                continue
            if block in owners:
                problems.append(
                    f"CROSS-LINK: block {block} owned by {owners[block]} and {index}"
                )
            else:
                owners[block] = index
            if not used(block):
                problems.append(f"inode {index}: block {block} not marked used in bitmap")
            if kind == 1 and slot * BLOCK >= size:
                problems.append(f"inode {index}: block {block} beyond size {size}")

        if double != 0:
            if not (data_start <= double < total):
                problems.append(f"inode {index}: double-indirect block {double} outside data area")
                continue
            if double in owners:
                problems.append(
                    f"CROSS-LINK: double-indirect {double} also owned by {owners[double]}"
                )
            else:
                owners[double] = f"{index} (double)"
            if not used(double):
                problems.append(f"inode {index}: double-indirect {double} not marked used")
            node = struct.unpack_from("<128I", image, double * BLOCK)
            for table_index, table in enumerate(node):
                if table == 0:
                    continue
                if not (data_start <= table < total):
                    problems.append(
                        f"inode {index} double[{table_index}]: table {table} outside data area"
                    )
                    continue
                if table in owners:
                    problems.append(
                        f"CROSS-LINK: table {table} owned by {owners[table]} and inode {index}"
                    )
                else:
                    owners[table] = f"{index} double[{table_index}]"
                if not used(table):
                    problems.append(f"inode {index}: table {table} not marked used")
                entries = struct.unpack_from("<128I", image, table * BLOCK)
                for pointer, block in enumerate(entries):
                    if block == 0:
                        continue
                    slot = DIRECT + 128 + table_index * 128 + pointer
                    if not (data_start <= block < total):
                        problems.append(
                            f"inode {index} double[{table_index}][{pointer}]: "
                            f"block {block} outside data area"
                        )
                        continue
                    if block in owners:
                        problems.append(
                            f"CROSS-LINK: block {block} owned by {owners[block]} and inode {index}"
                        )
                    else:
                        owners[block] = f"{index} double[{table_index}][{pointer}]"
                    if not used(block):
                        problems.append(f"inode {index}: block {block} not marked used in bitmap")
                    if kind == 1 and slot * BLOCK >= size:
                        problems.append(f"inode {index}: double block {block} beyond size {size}")

        if triple != 0:
            if not (data_start <= triple < total):
                problems.append(f"inode {index}: triple-indirect block {triple} outside data area")
                continue
            if triple in owners:
                problems.append(
                    f"CROSS-LINK: triple-indirect {triple} also owned by {owners[triple]}"
                )
            else:
                owners[triple] = f"{index} (triple)"
            if not used(triple):
                problems.append(f"inode {index}: triple-indirect {triple} not marked used")
            root = struct.unpack_from("<128I", image, triple * BLOCK)
            for outer, double_block in enumerate(root):
                if double_block == 0:
                    continue
                if not (data_start <= double_block < total):
                    problems.append(
                        f"inode {index} triple[{outer}]: double block {double_block} "
                        f"outside data area"
                    )
                    continue
                if double_block in owners:
                    problems.append(
                        f"CROSS-LINK: double block {double_block} owned by "
                        f"{owners[double_block]} and inode {index}"
                    )
                else:
                    owners[double_block] = f"{index} triple[{outer}]"
                if not used(double_block):
                    problems.append(
                        f"inode {index}: triple double block {double_block} not marked used"
                    )
                node = struct.unpack_from("<128I", image, double_block * BLOCK)
                for middle, table in enumerate(node):
                    if table == 0:
                        continue
                    if not (data_start <= table < total):
                        problems.append(
                            f"inode {index} triple[{outer}][{middle}]: table {table} "
                            f"outside data area"
                        )
                        continue
                    if table in owners:
                        problems.append(
                            f"CROSS-LINK: table {table} owned by {owners[table]} "
                            f"and inode {index}"
                        )
                    else:
                        owners[table] = f"{index} triple[{outer}][{middle}]"
                    if not used(table):
                        problems.append(f"inode {index}: table {table} not marked used")
                    entries = struct.unpack_from("<128I", image, table * BLOCK)
                    for pointer, block in enumerate(entries):
                        if block == 0:
                            continue
                        slot = TRIPLE_FIRST + outer * INDIRECT_SQUARED + middle * 128 + pointer
                        if not (data_start <= block < total):
                            problems.append(
                                f"inode {index} triple[{outer}][{middle}][{pointer}]: "
                                f"block {block} outside data area"
                            )
                            continue
                        if block in owners:
                            problems.append(
                                f"CROSS-LINK: block {block} owned by {owners[block]} "
                                f"and inode {index}"
                            )
                        else:
                            owners[block] = f"{index} triple[{outer}][{middle}][{pointer}]"
                        if not used(block):
                            problems.append(
                                f"inode {index}: block {block} not marked used in bitmap"
                            )
                        if kind == 1 and slot * BLOCK >= size:
                            problems.append(
                                f"inode {index}: triple block {block} beyond size {size}"
                            )

        if indirect == 0:
            continue
        if not (data_start <= indirect < total):
            problems.append(f"inode {index}: indirect block {indirect} outside data area")
            continue
        if indirect in owners:
            problems.append(
                f"CROSS-LINK: indirect table {indirect} also owned by {owners[indirect]}"
            )
        else:
            owners[indirect] = f"{index} (table)"
        if not used(indirect):
            problems.append(f"inode {index}: indirect table {indirect} not marked used")
        table = struct.unpack_from("<128I", image, indirect * BLOCK)
        for pointer, block in enumerate(table):
            if block == 0:
                continue
            slot = DIRECT + pointer
            if not (data_start <= block < total):
                problems.append(
                    f"inode {index} indirect[{pointer}]: block {block} outside data area"
                )
                continue
            if block in owners:
                problems.append(
                    f"CROSS-LINK: block {block} owned by {owners[block]} and inode {index}"
                )
            else:
                owners[block] = f"{index} indirect[{pointer}]"
            if not used(block):
                problems.append(f"inode {index}: block {block} not marked used in bitmap")
            if kind == 1 and slot * BLOCK >= size:
                problems.append(f"inode {index}: indirect block {block} beyond size {size}")

    leaked = [b for b in range(data_start, total) if used(b) and b not in owners]
    free = sum(1 for b in range(data_start, total) if not used(b))

    print(f"image: {path} ({len(image)} bytes, {total} blocks)")
    print(f"inodes: {files} files, {dirs} dirs; data blocks: {len(owners)} owned, {free} free")
    if leaked:
        problems.append(f"{len(leaked)} blocks marked used but unreferenced: {leaked[:12]}")
    if problems:
        print("PROBLEMS:")
        for problem in problems[:40]:
            print("  -", problem)
        return 1
    print("OK: no cross-links, bitmap consistent")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "disk.img"))
