#!/usr/bin/env python3
"""Build a PE32+/EFI wrapper around the Silver kernel Image (tablet EFI path).

ABL on SM8650 is a UEFI loader and boots the stock kernel (which starts with
'MZ') via LoadImage. This script wraps our arm64 Image into a minimal PE32+
EFI application:

  0x000  DOS header: 'MZ' + e_lfanew=0x40; the arm64 magic 'ARM\\x64' sits at
         0x38 inside the DOS padding so non-EFI checks still see it
  0x040  'PE\\0\\0' + COFF + PE32+ optional header (Subsystem=EFI app)
  0x200  file/RVA of the .text section: EFI entry first, kernel after it

The EFI entry (x0 = SystemTable) prints 'SILVER' via ConOut->OutputString,
then jumps into the kernel's real _start. ConOut works under the firmware's
own page tables, so the first observable signal no longer depends on our
guesses about the handover MMU state.
"""
import struct, sys

image = open('tablet/Image', 'rb').read()

# ---------------------------------------------------------------- EFI entry
def mov_reg(rd, rm):                       # mov Xd, Xm
    return 0xAA0003E0 | (rm << 16) | rd
def ldr_imm64(rt, rn, byte_off):           # ldr Xt, [Xn, #off]  (off % 8 == 0)
    return 0xF9400000 | ((byte_off // 8) << 10) | (rn << 5) | rt
def adr(rd, word_off):                     # adr Xd, #word_off*4
    imm = word_off & 0x3FFFFFF
    return 0x10000000 | ((imm & 3) << 29) | ((imm >> 2) << 5) | rd

code = bytearray()
def emit(w): code.extend(struct.pack('<I', w))

emit(mov_reg(23, 0))                       # x23 = SystemTable
emit(adr(24, 0))                           # x24 = load address of the payload
emit(ldr_imm64(25, 23, 64))                # x25 = ST->ConOut
emit(ldr_imm64(26, 25, 16))                # x26 = ConOut->OutputString

str_pos = len(code)                        # adr x27, <string>
emit(adr(27, 0))                           # patched below
emit(mov_reg(0, 25))                       # x0 = ConOut
emit(mov_reg(1, 27))                       # x1 = "SILVER BOOT\r\n"
emit(0xD63F0000 | (26 << 5))               # blr x26

ks_pos = len(code)                         # adr x16, <kernel _start>
emit(adr(16, 0))
emit(0xD61F0200)                           # br x16

string = ''.join(c + '\x00' for c in "SILVER BOOT\r\n").encode('utf-16-le')
code += string

# patch adrs: offsets are in words from the adr instruction itself
emit_pos = 4                                # second instruction (adr x24) at 4
struct.pack_into('<I', code, 4, adr(24, 0))
delta_str = (len(code) - str_pos - len(string)) // 4
struct.pack_into('<I', code, str_pos, adr(27, delta_str))
# kernel _start sits at (end of entry code + string) + 0x40 (its own header)
target_words = (len(code) - ks_pos + 0x40) // 4
struct.pack_into('<I', code, ks_pos, adr(16, target_words))

# ---------------------------------------------------------------- PE layout
SECTION_ALIGN = 0x200
FILE_ALIGN = 0x200
HEADERS_SIZE = 0x200
text_file_off = HEADERS_SIZE
text_rva = HEADERS_SIZE
size_of_image = (text_rva + len(code) + len(image) + SECTION_ALIGN - 1) & ~(SECTION_ALIGN - 1)

dos = bytearray(0x40)
dos[0:2] = b'MZ'
struct.pack_into('<I', dos, 0x3C, 0x40)            # e_lfanew
struct.pack_into('<I', dos, 0x38, 0x644D5241)      # arm64 magic "ARM\x64" (info)

pe = bytearray()
pe += b'PE\x00\x00'
# machine ARM64, 1 section, no symbols, optional header 0xF0, EXEC|LARGE
pe += struct.pack('<HHIIIHH', 0xAA64, 1, 0, 0, 0, 0xF0, 0x0022)

opt = bytearray()
opt += struct.pack('<H', 0x20B)                    # PE32+
opt += struct.pack('<B', 1) + struct.pack('<B', 0) # linker version
opt += struct.pack('<I', len(code) + len(image))   # SizeOfCode
opt += struct.pack('<I', 0) + struct.pack('<I', 0)
opt += struct.pack('<I', text_rva)                 # AddressOfEntryPoint
opt += struct.pack('<I', text_rva)                 # BaseOfCode
opt += struct.pack('<Q', 0)                        # ImageBase
opt += struct.pack('<I', SECTION_ALIGN)
opt += struct.pack('<I', FILE_ALIGN)
opt += struct.pack('<HHH', 0, 0, 0)
opt += struct.pack('<I', size_of_image)
opt += struct.pack('<I', HEADERS_SIZE)
opt += struct.pack('<I', 0)                        # CheckSum
opt += struct.pack('<HH', 10, 0)                   # Subsystem = EFI app
opt += struct.pack('<QQQQ', 0x100000, 0x1000, 0x100000, 0x1000)
opt += struct.pack('<I', 0) + struct.pack('<I', 16)
opt += bytes(16 * 8)
opt += bytes(0xF0 - len(opt))   # pad optional header to PE32+ size

sec = bytearray()
sec += b'.text\x00\x00\x00'
sec += struct.pack('<IIII', len(code) + len(image), text_rva,
                   len(code) + len(image), text_file_off)
sec += struct.pack('<IHHII', 0x60000020, 0, 0, 0, 0)  # CODE|EXECUTE|READ

headers = bytes(dos) + bytes(pe) + bytes(opt) + bytes(sec)
assert len(headers) <= HEADERS_SIZE, hex(len(headers))
headers += bytes(HEADERS_SIZE - len(headers))

out = headers + bytes(code) + image
open(sys.argv[1] if len(sys.argv) > 1 else 'tablet/Image.efi', 'wb').write(out)
print('EFI image:', len(out), 'bytes; entry RVA', hex(text_rva),
      '; EFI entry code:', len(code), 'bytes; kernel follows')
