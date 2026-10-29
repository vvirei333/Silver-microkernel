import struct
import os

# Форматирует disk.img в on-disk SFS: суперблок + битмап свободных блоков +
# таблицу inode. Данные начинаются с DATA_BLOCK_START; файлы создаются через
# sfs_server (touch / echo >).
#
# Layout (block size = 512 байт, всего 2048 блоков):
#   блок 0            суперблок (40 байт значащих, добивка до 64)
#   блок 1            битмап свободных блоков (2048 бит = 256 байт; 1 = занят)
#   блоки 2..17       таблица inode (128 inode x 64 байта = 8192 байта)
#   блоки 18..2047    блоки данных (2030 блоков)
DISK = 'disk.img'

if not os.path.exists(DISK):
    print("Ошибка: disk.img не найден. Создайте образ (run_disk.bat) или инициализируйте файл.")
    exit(1)

MAGIC = 0x53465321  # "SFS!"
VERSION = 1
BLOCK_SIZE = 512
TOTAL_BLOCKS = 2048
ROOT_INODE = 0
INODE_COUNT = 128

BITMAP_BLOCK = 1
INODE_TABLE_BLOCK = 2
INODE_TABLE_BLOCKS = 16  # 128 inode x 64 байта = 8192 байта = 16 блоков
DATA_BLOCK_START = 18

INODE_SIZE = 64
INODE_KIND_FREE = 0
INODE_KIND_DIR = 2

with open(DISK, 'r+b') as f:
    # 1. Суперблок: 10 полей u32 (40 байт) + 24 байта добивки = 64 байта.
    superblock = struct.pack('<IIIIIIIIII24x',
                             MAGIC, VERSION, BLOCK_SIZE, TOTAL_BLOCKS,
                             ROOT_INODE, INODE_COUNT,
                             BITMAP_BLOCK, INODE_TABLE_BLOCK,
                             INODE_TABLE_BLOCKS, DATA_BLOCK_START)
    f.seek(0)
    f.write(superblock)

    # 2. Битмап: 2048 бит. Бит установлен = блок занят. Метаданные (блоки
    #    0..DATA_BLOCK_START-1) помечаются занятыми.
    bitmap = bytearray(256)
    for block in range(DATA_BLOCK_START):
        bitmap[block // 8] |= 1 << (block % 8)
    f.seek(BITMAP_BLOCK * BLOCK_SIZE)
    f.write(bytes(bitmap))

    # 3. Таблица inode: inode 0 = корневой каталог (kind=dir, size=0).
    inode_table = bytearray(INODE_COUNT * INODE_SIZE)
    struct.pack_into('<II', inode_table, 0, INODE_KIND_DIR, 0)
    f.seek(INODE_TABLE_BLOCK * BLOCK_SIZE)
    f.write(bytes(inode_table))

    # 4. Блоки данных остаются как есть (битмап помечает их свободными).

    print("SUCCESS: SFS formatted (superblock + bitmap + inode table)")
    print(f"Magic: {hex(MAGIC)}  Block size: {BLOCK_SIZE}  Blocks: {TOTAL_BLOCKS}")
    print(f"Bitmap: block {BITMAP_BLOCK}  Inode table: blocks "
          f"{INODE_TABLE_BLOCK}..{INODE_TABLE_BLOCK + INODE_TABLE_BLOCKS - 1}  "
          f"Data from block {DATA_BLOCK_START}")
