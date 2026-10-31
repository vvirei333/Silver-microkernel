#!/usr/bin/env python3
# Adapted from tb321fu-linux tools/repartition-gpt.py for the **256 GB**
# TB321FU (60,884,992 x 4096B sectors). The upstream script hardcodes the
# 512 GB geometry (backup GPT at LBA 123365374) and would write past our
# disk. This one reads the CURRENT GPT dump, shrinks userdata to ~108 GiB,
# renames our experimental Y700BOOT (sda14) to SILVERBOOT, and creates the
# port's Y700BOOT (1 GiB, sda17) + tb321fu-root (sda18) in the freed space.
import struct, uuid, zlib, sys

src = sys.argv[1] if len(sys.argv) > 1 else '/tmp/gpt_cur.bin'
SS = 4096
d = open(src, 'rb').read()

hdr = bytearray(d[SS:SS+92])
sig, rev, hsz, crc, res, cur, bkp, first, last, guid, plba, nparts, psz, pcrc = \
    struct.unpack('<8sIIIIQQQQ16sQIII', hdr)
assert sig == b'EFI PART'
assert bkp == 60884991, f'unexpected disk geometry: backup LBA {bkp}'
bkp_arr_lba = 60884987

arr = bytearray(d[plba*SS : plba*SS + nparts*psz])

def get(i):  return arr[i*psz:(i+1)*psz]
def put(i, tg, pg, first_, last_, flags, name):
    tb = uuid.UUID(tg).bytes_le
    pb = uuid.UUID(pg).bytes_le
    nb = name.encode('utf-16le').ljust(72, b'\x00')[:72]
    arr[i*psz:(i+1)*psz] = tb + pb + struct.pack('<QQQ', first_, last_, flags) + nb

def name_of(i):
    return get(i)[56:128].decode('utf-16le').rstrip('\x00')

# 1. sda14 (idx 13): our Y700BOOT -> SILVERBOOT (same LBAs, data untouched)
assert name_of(13) == 'Y700BOOT', name_of(13)
e = get(13)
put(13, str(uuid.UUID(bytes_le=bytes(e[:16]))), str(uuid.UUID(bytes_le=bytes(e[16:32]))),
    *struct.unpack('<QQQ', e[32:56]), 'SILVERBOOT')

# 2. sda16 (idx 15): userdata 218.47 GiB -> 108 GiB
USERDATA_SECTORS = 108 * 1024**3 // 4096          # 28,997,529.6 -> keep exact below
USERDATA_SECTORS = 108 * 1024 * 1024 * 1024 // 4096
u_first = 3613096
u_last = u_first + USERDATA_SECTORS - 1
e = get(15)
assert name_of(15) == 'userdata', name_of(15)
put(15, str(uuid.UUID(bytes_le=bytes(e[:16]))), str(uuid.UUID(bytes_le=bytes(e[16:32]))),
    u_first, u_last, struct.unpack('<Q', e[48:56])[0], 'userdata')
print(f'userdata: {u_first}..{u_last} ({USERDATA_SECTORS*4096/1024**3:.2f} GiB)')

# 3. sda17 (idx 16): Y700BOOT 1 GiB (EFI System Partition)
y_first, y_last = u_last + 1, u_last + 262144
put(16, 'c12a7328-f81f-11d2-ba4b-00a0c93ec93b', str(uuid.uuid4()),
    y_first, y_last, 0, 'Y700BOOT')
print(f'Y700BOOT: {y_first}..{y_last} ({(y_last-y_first+1)*4096/1024**3:.2f} GiB)')

# 4. sda18 (idx 17): tb321fu-root — everything up to LastUsable
r_first, r_last = y_last + 1, last
put(17, '0fc63daf-8483-4772-8e79-3d69d8477de4', str(uuid.uuid4()),
    r_first, r_last, 0, 'tb321fu-root')
print(f'tb321fu-root: {r_first}..{r_last} ({(r_last-r_first+1)*4096/1024**3:.2f} GiB)')
assert r_last <= last and r_last <= bkp_arr_lba - 1

# 5. CRCs: array, then primary header (crc field zeroed while computing)
arr_crc = zlib.crc32(arr)
pri = bytearray(hdr)
struct.pack_into('<I', pri, 88, arr_crc)
struct.pack_into('<I', pri, 16, 0)
pri_crc = zlib.crc32(pri)
struct.pack_into('<I', pri, 16, pri_crc)

# 6. Backup header: mirrors primary but cur/bkp/entry LBA swapped
# Header layout: 24 CurrentLBA, 32 BackupLBA, 40 FirstUsable, 48 LastUsable,
# 56 DiskGUID, 72 PartitionEntryLBA, 80 NumEntries, 84 EntrySize, 88 ArrayCRC
bkp_hdr = bytearray(pri)
struct.pack_into('<I', bkp_hdr, 16, 0)          # recalc CRC for backup
struct.pack_into('<Q', bkp_hdr, 24, bkp)        # CurrentLBA = backup
struct.pack_into('<Q', bkp_hdr, 32, 1)          # BackupLBA = primary
struct.pack_into('<Q', bkp_hdr, 72, bkp_arr_lba)
bkp_crc = zlib.crc32(bkp_hdr)
struct.pack_into('<I', bkp_hdr, 16, bkp_crc)

out_primary = bytes(d[0:SS]) + bytes(pri).ljust(SS, b'\x00') + bytes(arr)      # LBA 0,1,2
out_backup = bytes(arr) + bytes(bkp_hdr).ljust(SS, b'\x00')                    # LBA 60884987, 60884991

open('/tmp/gpt_new_primary.bin', 'wb').write(out_primary)
open('/tmp/gpt_new_backup.bin', 'wb').write(out_backup)
print(f'array CRC {arr_crc:#x}, primary hdr CRC {pri_crc:#x}, backup hdr CRC {bkp_crc:#x}')
print('WROTE /tmp/gpt_new_primary.bin (3 sectors) and /tmp/gpt_new_backup.bin')
