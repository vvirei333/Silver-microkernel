import struct
import os

# Форматирует disk.img в on-disk SFS: суперблок + битмап свободных блоков +
# таблицу inode. Данные начинаются с DATA_BLOCK_START; файлы создаются через
# sfs_server (touch / echo >).
#
# Layout (block size = 512 байт, всего 2048 блоков):
#   блок 0            суперблок (40 байт значащих, добивка до 64)
#   блок 1            битмап свободных блоков (2048 бит = 256 байт; 1 = занят)
#   блоки 2..65       таблица inode (512 inode x 64 байта = 32768 байт)
#   блоки 66..2047    блоки данных (1982 блока)
DISK = 'disk.img'

if not os.path.exists(DISK):
    print("Ошибка: disk.img не найден. Создайте образ (run_disk.bat) или инициализируйте файл.")
    exit(1)

MAGIC = 0x53465321  # "SFS!"
VERSION = 1
BLOCK_SIZE = 512
TOTAL_BLOCKS = 2048
ROOT_INODE = 0
INODE_COUNT = 512

BITMAP_BLOCK = 1
INODE_TABLE_BLOCK = 2
INODE_TABLE_BLOCKS = 64  # 512 inode x 64 байта = 32768 байт = 64 блока
DATA_BLOCK_START = 66

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
#
# Inode (64 байта): kind u32 @0, size u32 @4, direct[4] @8..24,
# indirect u32 @24 (0 = нет), остальное зарезервировано. Один indirect-блок
# хранит 128 указателей, поэтому файл занимает до 4 + 128 = 132 блоков
# (66 КиБ).

    print("SUCCESS: SFS formatted (superblock + bitmap + inode table)")
    print(f"Magic: {hex(MAGIC)}  Block size: {BLOCK_SIZE}  Blocks: {TOTAL_BLOCKS}")
    print(f"Bitmap: block {BITMAP_BLOCK}  Inode table: blocks "
          f"{INODE_TABLE_BLOCK}..{INODE_TABLE_BLOCK + INODE_TABLE_BLOCKS - 1}  "
          f"Data from block {DATA_BLOCK_START}")
