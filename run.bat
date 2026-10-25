@echo off
cargo build --target aarch64-unknown-none --no-default-features --features no_std
"C:\Program Files\qemu\qemu-system-aarch64.exe" -machine virt -cpu cortex-a72 -m 128M -nographic -kernel target/aarch64-unknown-none/debug/silver_microkernel