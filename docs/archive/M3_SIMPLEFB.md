# M3.2a — simplefb boot framebuffer

Minimal framebuffer path for the Redmi first picture. It is presentation only:
no GPU, DRM, compositor, surfaces or input.

## Contract

`simplefb::Info` is what a bootloader/DT must provide:

| Field | Meaning |
|---|---|
| `base` | physical framebuffer base |
| `width`, `height` | visible pixels |
| `stride` | pixels per row (may exceed `width`) |
| `format` | `Argb8888` (`0xAARRGGBB` in a little-endian u32) |

The Redmi 10C `cont_splash_region` proved during the previous bring-up is
`0x5C00_0000`, 720x1650, stride 720, ARGB8888. That is the `FOG_PANEL`
constant.

## What landed

- `src/simplefb.rs` (shared lib): a pure `Canvas` over `&mut [u32]` with
  strict `stride >= width`, `stride * height <= pixels.len()` validation,
  clipped `fill_rect`, bounds-checked `put`, and a boot panel (vertical
  gradient, accent band, card, pale disc). Writes never touch the stride
  padding.
- Kernel boot path (`simplefb_boot` in `main.rs`):
  - **Fog:** maps `0x5C00_0000` through the existing high-half mapping and
    paints `FOG_PANEL`. Logs `SIMPLEFB: FOG base=... 720x1650 stride=720`.
  - **QEMU and other non-fog profiles:** allocates a 128x96 synthetic surface
    from the frame allocator, paints it, verifies the first pixel and the
    accent band, frees the frames and logs `SIMPLEFB TEST PASS 128x96`.
- Host tests (`cargo test --lib`): canvas validation/padding and the boot
  panel's gradient/band/disc. Total is now **226**.

## Verification

- `SIMPLEFB TEST PASS 128x96` on the QEMU boot.
- security 12/12 + post-suite health + 200k fuzz.
- Linux ABI matrix 26/26.
- Phosh demo clean, health 7 live / 0 zombie / DMA 0/0.
- `cargo build --features no_std,fog` compiles the Redmi path.
- `--no-linux` clean.

The Fog panel has **not** run on hardware in this slice; that is tomorrow's
Redmi boot test.

## Limits / next

- The kernel paints one boot panel. There is no EL0 framebuffer API and no
  compositor yet; that comes after the hardware picture is proven.
- The framebuffer descriptor is a constant, not parsed from DT
  (`/chosen`/`simple-framebuffer`, reserved-memory `cont_splash`). The Redmi
  handover may provide base/size/stride/format; validate it before trusting.
- Format conversion is ARGB8888 only.
- M3.2 still needs eMMC/UFS, HID/touch and I2C-mini.
