import struct
import os

# Проверяем, существует ли файл диска
if not os.path.exists('disk.img'):
    print("Ошибка: disk.img не найден. Запустите run_disk.bat или создайте образ.")
    exit(1)

# SFS Superblock (64 байта)
# <IIIIII означает 6 беззнаковых int (по 4 байта), 40x - добивка нулями до 64 байт
MAGIC = 0x53465321  # "SFS!"
VERSION = 1
BLOCK_SIZE = 512
TOTAL_BLOCKS = 2048
ROOT_INODE = 1
INODE_COUNT = 128

# Формируем бинарную структуру
superblock = struct.pack(
    '<IIIIII40x', 
    MAGIC, 
    VERSION, 
    BLOCK_SIZE, 
    TOTAL_BLOCKS, 
    ROOT_INODE, 
    INODE_COUNT
)

# Записываем в начало диска (сектор 0)
with open('disk.img', 'r+b') as f:
    f.seek(0)
    f.write(superblock)
    print("SUCCESS: SFS Superblock written to disk.img!")
    print(f"Magic: {hex(MAGIC)}")
    print(f"Block Size: {BLOCK_SIZE}")
    print(f"Total Blocks: {TOTAL_BLOCKS}")