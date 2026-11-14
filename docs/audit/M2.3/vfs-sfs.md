# Entry point 6 — VFS and the SFS on-disk format

Audited code snapshot: `snapshot/` (current `main`). SFS is the read/write
rootfs served by the kernel; `vfs.rs` is the path layer above it and
`sfs_format.rs` parses the bytes on disk. Both are reachable from EL0 by
reading, writing, creating and renaming files (subject to `CAP_FS_WRITE` or a
bundle scope prefix).

## The path

- **VFS** (`vfs.rs`): the `Vfs` trait (`read_at`, `write_at`, `read_dir`,
  `create_file`, `remove_*`, `rename`, `seed_file`) and the concrete
  `SfsFs`/`RamFs` implementations. Paths are resolved below the mount point;
  the shell and the Linux ABI (`open`, `stat`, `getdents`, `unlink`, `rename`)
  call into it.
- **SFS format** (`sfs_format.rs`): `superblock()` (magic/geometry),
  `inode_get`/`inode_put`, `block_start`, the direct + single/double/triple
  indirect block walk, `resolve_dir`/`lookup`, `create_inode`,
  `read_inode_bytes`, `free_inode_blocks`, `alloc_block`/`free_blocks`, the
  bitmap and directory block management.
- **`sfs.rs`** connects the cached image bytes to a `SfsDisk` (virtio-blk or
  the in-memory test disk) and implements the VFS trait.
- **`elf.rs`** (in the snapshot) is the other parser of untrusted bytes: the
  ELF header/program headers/BSS of a file loaded from `/sfs`.

## Trust boundary

The disk image is **untrusted input**: it can be replaced, and bundle
installation writes files into it. A corrupt superblock, a cyclic indirect
block, a dirent with a huge name or a block list pointing at the superblock
itself must produce an error, never a panic and never a free/write outside the
image. Reads and writes are bounded by the image length and the 8 MiB
whole-file cap.

## Leads for this packet

- **V1 — superblock validation.** `superblock()` must check magic, version,
  block size, inode/block counts and that `inode_count * inode_size` and
  `block_count * block_size` fit the image and cannot overflow `usize`
  (multiply/divide order matters on 32-bit-sized values).
- **V2 — inode offsets and block addresses.** `inode_offset_checked` and
  `block_start` are the choke points; verify every caller uses them, that the
  inode number is `< inode_count`, and that a block address is
  `< block_count` before indexing the cached image.
- **V3 — indirect block chains.** Direct, single, double and triple indirect
  levels parse block-number arrays from disk. Check counts against the inode
  size, that a block cannot list itself (or a cycle) and loop forever, that
  duplicate blocks cannot be double-freed, and that the third level's
  arithmetic cannot overflow.
- **V4 — directory entries.** Name length versus the block, NUL termination,
  `.`/`..` handling, duplicate names, and a directory whose own block is also
  reachable through another inode. Path resolution (`resolve_dir`) must not
  follow `..` above the image root.
- **V5 — read/write arithmetic.** `read_at(path, offset, limit)` and
  `write_at`: offset+limit overflow, limit larger than the file, sparse files
  (holes), and `usage_under` walking a hostile tree with recursion/visited
  protection.
- **V6 — free/alloc bitmap.** `free_blocks` on a list taken from a corrupt
  inode can mark the superblock, inode table or another file's blocks free.
  Check the bounds before every bitmap write and that `alloc_block` cannot
  hand out block 0 or a metadata block.
- **V7 — rename/replace rollback.** The cross-directory move and
  replace-over-destination paths must not lose either name when a step fails;
  check the quota/scope (`CAP_FS_WRITE`, bundle prefix) is rechecked at the
  destination.
- **V8 — ELF parsing.** `elf.rs` reads an arbitrary file: check `e_phoff`/
  `e_phnum`/`p_offset`/`p_filesz`/`p_memsz` bounds, BSS zeroing past the file,
  segment overlap, and that a 32 MiB streamed image cannot overflow the frame
  ledger.
