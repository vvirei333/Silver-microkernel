@echo off
echo Creating test disk image...
powershell -Command "$disk = New-Object byte[] 1048576; $disk[0]=0xDE; $disk[1]=0xAD; $disk[2]=0xBE; $disk[3]=0xEF; [System.IO.File]::WriteAllBytes('disk.img', $disk)"

echo Starting QEMU with Virtio-blk...
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 128M -nographic ^
  -kernel target/aarch64-unknown-none/debug/silver_microkernel ^
  -drive id=hd0,file=disk.img,if=none,format=raw ^
  -device virtio-blk-pci,drive=hd0,disable-legacy=on