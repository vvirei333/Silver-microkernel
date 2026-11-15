//! Minimal boot framebuffer: the "simplefb" contract (a base, stride, size
//! and pixel format provided by the bootloader). This is presentation only;
//! no GPU, DRM, surfaces or input.
//!
//! The canvas is pure and host-tested. The kernel boot path maps the physical
//! base and paints one panel: on the Redmi `cont_splash` is 0x5C00_0000,
//! 720x1650, stride 720, 32-bit ARGB; on QEMU a small synthetic surface
//! validates the writer without a display.

/// Pixel format of a simple framebuffer. `Argb8888` stores `0xAARRGGBB` in a
/// little-endian u32, the same layout the Redmi splash region uses.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    Argb8888,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Info {
    pub base: u64,
    pub width: usize,
    pub height: usize,
    /// Pixels per row; may be larger than `width` for alignment.
    pub stride: usize,
    pub format: PixelFormat,
}

impl Info {
    pub fn row_bytes(self) -> Option<usize> {
        self.stride.checked_mul(4)
    }

    pub fn bytes(self) -> Option<usize> {
        self.stride.checked_mul(self.height)?.checked_mul(4)
    }
}

/// The Redmi 10C `cont_splash_region` proved during the previous bring-up.
pub const FOG_PANEL: Info = Info {
    base: 0x5C00_0000,
    width: 720,
    height: 1650,
    stride: 720,
    format: PixelFormat::Argb8888,
};

/// A window into a packed 32-bit framebuffer. Writes outside `width`/`height`
/// are ignored; the stride padding is never touched.
pub struct Canvas<'a> {
    pixels: &'a mut [u32],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

impl<'a> Canvas<'a> {
    pub fn new(
        pixels: &'a mut [u32],
        width: usize,
        height: usize,
        stride: usize,
    ) -> Option<Self> {
        if width == 0 || height == 0 || stride < width {
            return None;
        }
        if stride.checked_mul(height)? > pixels.len() {
            return None;
        }
        Some(Self {
            pixels,
            width,
            height,
            stride,
        })
    }

    pub fn fill(&mut self, color: u32) {
        for row in 0..self.height {
            let start = row * self.stride;
            self.pixels[start..start + self.width].fill(color);
        }
    }

    /// Fills `[x, x+w) x [y, y+h)` clipped to the canvas.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        if x >= self.width || y >= self.height || w == 0 || h == 0 {
            return;
        }
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        for row in y..y1 {
            let start = row * self.stride + x;
            self.pixels[start..start + (x1 - x)].fill(color);
        }
    }

    pub fn put(&mut self, x: usize, y: usize, color: u32) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        self.pixels[y * self.stride + x] = color;
        true
    }

    /// A vertical RGB gradient with a horizontal accent band and a moon-like
    /// disc. Enough to prove the panel path on a UART-less device.
    pub fn paint_boot(&mut self) {
        let top = (10u32, 16u32, 30u32);
        let bottom = (3u32, 5u32, 10u32);
        for row in 0..self.height {
            let t = if self.height > 1 { (row * 255 / (self.height - 1)) as i64 } else { 0 };
            let red = top.0 as i64 + (bottom.0 as i64 - top.0 as i64) * t / 255;
            let green = top.1 as i64 + (bottom.1 as i64 - top.1 as i64) * t / 255;
            let blue = top.2 as i64 + (bottom.2 as i64 - top.2 as i64) * t / 255;
            let color = 0xFF00_0000
                | ((red as u32) << 16)
                | ((green as u32) << 8)
                | blue as u32;
            self.fill_rect(0, row, self.width, 1, color);
        }
        // Accent band and a centred card: unmistakable on a photo.
        let band = self.height / 3;
        self.fill_rect(self.width / 10, band, self.width * 8 / 10, 8, 0xFF39_D2FF);
        let card_w = self.width * 3 / 5;
        let card_h = self.height / 6;
        self.fill_rect(
            (self.width - card_w) / 2,
            band + self.height / 8,
            card_w,
            card_h,
            0xFF11_1826,
        );
        // A pale disc in the upper-right, the same motif as the wallpaper.
        let radius = self.width / 8;
        let cx = self.width * 3 / 4;
        let cy = self.height / 4;
        let r2 = radius * radius;
        let left = cx.saturating_sub(radius);
        let top = cy.saturating_sub(radius);
        for dy in 0..=radius * 2 {
            for dx in 0..=radius * 2 {
                let x = left + dx;
                let y = top + dy;
                let ox = dx as isize - radius as isize;
                let oy = dy as isize - radius as isize;
                if (ox * ox + oy * oy) as usize <= r2 {
                    let _ = self.put(x, y, 0xFFE8_ECF8);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Canvas, FOG_PANEL};

    #[test]
    fn canvas_validates_stride_and_bounds() {
        let mut pixels = [0xFFFF_FFFFu32; 5 * 3];
        assert!(Canvas::new(&mut pixels, 4, 3, 2).is_none());
        assert!(Canvas::new(&mut pixels, 4, 3, 6).is_none());
        let mut canvas = Canvas::new(&mut pixels, 4, 3, 5).unwrap();
        canvas.fill(0xFF00_00FF);
        assert_eq!(canvas.put(0, 0, 0xFF12_3456), true);
        assert_eq!(canvas.put(4, 0, 0xFF12_3456), false);
        assert_eq!(canvas.put(0, 3, 0xFF12_3456), false);
        assert_eq!(canvas.pixels[0], 0xFF12_3456);
        // The stride padding belongs to nobody.
        assert_eq!(canvas.pixels[4], 0xFFFF_FFFF);
        assert_eq!(canvas.pixels[5 * 2 + 4], 0xFFFF_FFFF);
    }

    #[test]
    fn boot_panel_paints_gradient_band_and_disc() {
        let mut pixels = [0u32; 64 * 48];
        let mut canvas = Canvas::new(&mut pixels, 64, 48, 64).unwrap();
        canvas.paint_boot();
        // Top and bottom rows differ; the accent band is exact; the disc is
        // pale in the upper-right.
        assert_ne!(pixels[0], pixels[64 * 47]);
        assert_eq!(pixels[64 * (48 / 3) + 32], 0xFF39_D2FF);
        assert_eq!(pixels[64 * 12 + 48], 0xFFE8_ECF8);
        assert_eq!(FOG_PANEL.width, 720);
        assert_eq!(FOG_PANEL.bytes(), Some(720 * 1650 * 4));
    }
}
