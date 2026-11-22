//! Skins: "standard" (the system look) and "play" (dark glass, a blue-violet-pink gradient,
//! the Rubik font). The settings page restyles itself from `settings.skin`; the cards drawn
//! here (notifications, the marker title card, the recording pill) take their colours, font
//! and shapes from this module.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{AddFontMemResourceEx, HDC};
use windows::Win32::Graphics::GdiPlus::{
    GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC, GdipCreateLineBrushI, GdipCreatePath, GdipCreatePen2,
    GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDrawPath,
    GdipFillPath, GdipSetPixelOffsetMode, GdipSetSmoothingMode, GdiplusStartup, GdiplusStartupInput, GpBrush,
    GpGraphics, GpPath, FillModeAlternate, PixelOffsetModeHalf, Point, SmoothingModeAntiAlias, UnitPixel, WrapModeTile,
};

const RUBIK: &[u8] = include_bytes!("../ui/Rubik.ttf");

static PLAY: AtomicU8 = AtomicU8::new(0);
static READY: AtomicBool = AtomicBool::new(false);

/// Loads the bundled font (for this process only) and starts GDI+. Once, at startup.
pub fn init() {
    if READY.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let fonts = 0u32;
        let _ = AddFontMemResourceEx(RUBIK.as_ptr() as *const _, RUBIK.len() as u32, None, &fonts);
        let input = GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
        let mut token = 0usize;
        let _ = GdiplusStartup(&mut token, &input, std::ptr::null_mut());
    }
}

/// Follows the setting ("play" or anything else = standard).
pub fn set(name: &str) {
    PLAY.store((name == "play") as u8, Ordering::Relaxed);
}

pub fn play() -> bool {
    PLAY.load(Ordering::Relaxed) == 1
}

/// Colours of the drawn cards, as COLORREF (0x00BBGGRR).
#[derive(Clone, Copy)]
pub struct Palette {
    pub background: u32,
    pub field: u32,
    pub border: u32,
    pub title: u32,
    pub detail: u32,
    pub accent: u32,
    pub button: u32,
    pub button_hover: u32,
    pub red: u32,
    pub quiet: u32,
}

const STANDARD: Palette = Palette {
    background: 0x001F_1F1F,
    field: 0x002C_2C2C,
    border: 0x003A_3A3A,
    title: 0x00FF_FFFF,
    detail: 0x00B4_B0AE,
    accent: 0x00FF_840A, // #0a84ff
    button: 0x0033_3333,
    button_hover: 0x0047_4747,
    red: 0x003A_45FF,
    quiet: 0x00A1_9C9C,
};

const PLAY_COLORS: Palette = Palette {
    background: 0x0016_100F,   // #0f1016
    field: 0x0025_201E,        // #1e2025
    border: 0x0047_3B37,       // #373b47
    title: 0x00F6_F0EE,        // #eef0f6
    detail: 0x00C3_A8A3,       // #a3a8c3
    accent: 0x00F2_708F,       // #8f70f2
    button: 0x002E_2A28,       // #282a2e
    button_hover: 0x003C_3734, // #34373c
    red: 0x0054_47F7,          // #f74754
    quiet: 0x00C3_A8A3,
};

pub fn palette() -> Palette {
    if play() {
        PLAY_COLORS
    } else {
        STANDARD
    }
}

/// The text font of the drawn cards.
pub fn face() -> PCWSTR {
    if play() {
        w!("Rubik")
    } else {
        w!("Segoe UI Variable Text")
    }
}

/// The gradient of the skin, left to right: blue, violet, pink (ARGB).
pub const GRADIENT: (u32, u32) = (0xFF38_5CF2, 0xFFF2_94D1);
/// The softer gradient of rings and outlines (ARGB).
pub const RING: (u32, u32) = (0xFF4A_6BEB, 0xFFED_99DB);

/// What fills a shape.
#[derive(Clone, Copy)]
pub enum Fill {
    /// One colour, ARGB.
    Solid(u32),
    /// Left to right across the shape, ARGB.
    Gradient(u32, u32),
}

/// COLORREF (0x00BBGGRR) to opaque ARGB.
pub fn argb(c: u32) -> u32 {
    0xFF00_0000 | (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16 & 0xFF)
}

struct Gfx(*mut GpGraphics);

impl Gfx {
    unsafe fn new(dc: HDC) -> Option<Self> {
        let mut g: *mut GpGraphics = std::ptr::null_mut();
        if GdipCreateFromHDC(dc, &mut g).0 != 0 || g.is_null() {
            return None;
        }
        let _ = GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        let _ = GdipSetPixelOffsetMode(g, PixelOffsetModeHalf);
        Some(Self(g))
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe {
            let _ = GdipDeleteGraphics(self.0);
        }
    }
}

/// A rounded rectangle path; `inset` moves the edges in (for strokes centred on the line).
unsafe fn rounded(r: &RECT, radius: f32, inset: f32) -> *mut GpPath {
    let mut path: *mut GpPath = std::ptr::null_mut();
    let _ = GdipCreatePath(FillModeAlternate, &mut path);
    let (x, y) = (r.left as f32 + inset, r.top as f32 + inset);
    let (w, h) = ((r.right - r.left) as f32 - 2.0 * inset, (r.bottom - r.top) as f32 - 2.0 * inset);
    let d = (2.0 * radius).min(w).min(h).max(0.5);
    let _ = GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    let _ = GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
    let _ = GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
    let _ = GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
    let _ = GdipClosePathFigure(path);
    path
}

unsafe fn brush(r: &RECT, fill: Fill) -> *mut GpBrush {
    match fill {
        Fill::Solid(c) => {
            let mut b = std::ptr::null_mut();
            let _ = GdipCreateSolidFill(c, &mut b);
            b as *mut GpBrush
        }
        Fill::Gradient(a, z) => {
            let mut b = std::ptr::null_mut();
            let p1 = Point { X: r.left - 1, Y: r.top };
            let p2 = Point { X: r.right + 1, Y: r.bottom };
            let _ = GdipCreateLineBrushI(&p1, &p2, a, z, WrapModeTile, &mut b);
            b as *mut GpBrush
        }
    }
}

/// Fills a rounded rectangle, smoothed.
pub unsafe fn fill_round(dc: HDC, r: RECT, radius: f32, fill: Fill) {
    let Some(g) = Gfx::new(dc) else { return };
    let path = rounded(&r, radius, 0.0);
    let b = brush(&r, fill);
    let _ = GdipFillPath(g.0, b, path);
    let _ = GdipDeleteBrush(b);
    let _ = GdipDeletePath(path);
}

/// Draws a rounded outline of `width` pixels inside `r`, smoothed.
pub unsafe fn ring(dc: HDC, r: RECT, radius: f32, width: f32, fill: Fill) {
    let Some(g) = Gfx::new(dc) else { return };
    let path = rounded(&r, (radius - width / 2.0).max(0.0), width / 2.0);
    let b = brush(&r, fill);
    let mut pen = std::ptr::null_mut();
    let _ = GdipCreatePen2(b, width, UnitPixel, &mut pen);
    let _ = GdipDrawPath(g.0, pen, path);
    let _ = GdipDeletePen(pen);
    let _ = GdipDeleteBrush(b);
    let _ = GdipDeletePath(path);
}

/// The corner radius Windows 11 gives a rounded window, in pixels at this DPI.
pub fn window_radius(dpi: u32) -> f32 {
    8.0 * dpi as f32 / 96.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorref_to_argb() {
        assert_eq!(argb(0x00F2_708F), 0xFF8F_70F2);
        assert_eq!(argb(0x0016_100F), 0xFF0F_1016);
    }
}
