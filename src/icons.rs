//! Tray icons, drawn in code (smooth at any size, no image files): a ring that is dim when
//! everything is off, gets a dot when the replay buffer runs, and a red dot while recording.

use std::ffi::c_void;
use std::path::Path;

use anyhow::Result;
use windows::core::w;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconFromResourceEx, HICON, LR_DEFAULTCOLOR};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Off = 0,
    Buffer = 1,
    Recording = 2,
}

pub const ALL: [State; 3] = [State::Off, State::Buffer, State::Recording];

/// True when the taskbar is light (the icon then has to be dark).
pub fn taskbar_is_light() -> bool {
    let mut value = 0u32;
    let mut size = 4u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut c_void),
            Some(&mut size),
        )
        .is_ok()
            && value != 0
    }
}

/// BGRA pixels with straight alpha, top row first.
pub fn pixels(state: State, size: u32, light_taskbar: bool) -> Vec<u8> {
    let ink: [f32; 3] = if light_taskbar { [0.10, 0.10, 0.10] } else { [1.0, 1.0, 1.0] };
    let red: [f32; 3] = [1.0, 0.27, 0.23];
    let (ring_alpha, dot_radius, dot_color) = match state {
        State::Off => (0.55, 0.0, ink),
        State::Buffer => (1.0, 0.17, ink),
        State::Recording => (1.0, 0.22, red),
    };
    let (outer, inner) = (0.44f32, 0.33f32);
    const SUB: u32 = 4;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (mut ring, mut dot) = (0u32, 0u32);
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let px = (x as f32 + (sx as f32 + 0.5) / SUB as f32) / size as f32 - 0.5;
                    let py = (y as f32 + (sy as f32 + 0.5) / SUB as f32) / size as f32 - 0.5;
                    let d = (px * px + py * py).sqrt();
                    if d <= outer && d >= inner {
                        ring += 1;
                    } else if d <= dot_radius {
                        dot += 1;
                    }
                }
            }
            let n = (SUB * SUB) as f32;
            let (ar, ad) = (ring as f32 / n * ring_alpha, dot as f32 / n);
            let alpha = ar + ad;
            if alpha > 0.0 {
                let i = ((y * size + x) * 4) as usize;
                for c in 0..3 {
                    let v = (ink[c] * ar + dot_color[c] * ad) / alpha;
                    out[i + 2 - c] = (v * 255.0).round() as u8; // RGB -> BGR order
                }
                out[i + 3] = (alpha.min(1.0) * 255.0).round() as u8;
            }
        }
    }
    out
}

/// The program's own icon (exe, Start menu, taskbar): a dark rounded tile with the ring and
/// a red record dot. BGRA pixels with straight alpha, top row first.
pub fn app_pixels(size: u32) -> Vec<u8> {
    const SUB: u32 = 4;
    let top = [0.30f32, 0.30, 0.32];
    let bottom = [0.10f32, 0.10, 0.11];
    let white = [1.0f32, 1.0, 1.0];
    let red = [1.0f32, 0.27, 0.23];
    // Tile: a superellipse (the "squircle" of macOS icons) filling 92 % of the canvas.
    let half = 0.46f32;
    let inside_tile = |x: f32, y: f32| (x.abs() / half).powf(5.0) + (y.abs() / half).powf(5.0) <= 1.0;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0f32; 3];
            let mut cover = 0f32;
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let px = (x as f32 + (sx as f32 + 0.5) / SUB as f32) / size as f32 - 0.5;
                    let py = (y as f32 + (sy as f32 + 0.5) / SUB as f32) / size as f32 - 0.5;
                    if !inside_tile(px, py) {
                        continue;
                    }
                    let d = (px * px + py * py).sqrt();
                    let t = (py + half) / (2.0 * half);
                    let colour = if d <= 0.125 {
                        red
                    } else if (0.225..=0.30).contains(&d) {
                        white
                    } else {
                        [0, 1, 2].map(|c| top[c] + (bottom[c] - top[c]) * t)
                    };
                    for c in 0..3 {
                        sum[c] += colour[c];
                    }
                    cover += 1.0;
                }
            }
            if cover > 0.0 {
                let i = ((y * size + x) * 4) as usize;
                for c in 0..3 {
                    out[i + 2 - c] = (sum[c] / cover * 255.0).round() as u8;
                }
                out[i + 3] = (cover / (SUB * SUB) as f32 * 255.0).round() as u8;
            }
        }
    }
    out
}

/// One icon image as Windows stores it: header, colour rows bottom-up, then a 1-bit mask.
fn image_data(px: &[u8], size: u32) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let mut data = Vec::with_capacity(40 + px.len() + (mask_row * size) as usize);
    data.extend_from_slice(&40u32.to_le_bytes());
    data.extend_from_slice(&(size as i32).to_le_bytes());
    data.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&32u16.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&(px.len() as u32).to_le_bytes());
    data.extend_from_slice(&[0u8; 16]);
    for y in (0..size).rev() {
        let row = (y * size * 4) as usize;
        data.extend_from_slice(&px[row..row + (size * 4) as usize]);
    }
    data.resize(data.len() + (mask_row * size) as usize, 0);
    data
}

pub fn make(state: State, size: u32, light_taskbar: bool) -> Result<HICON> {
    let data = image_data(&pixels(state, size, light_taskbar), size);
    let icon = unsafe { CreateIconFromResourceEx(&data, true, 0x0003_0000, size as i32, size as i32, LR_DEFAULTCOLOR)? };
    Ok(icon)
}

/// Writes the program icon as an .ico file with every size Windows asks for. Run once
/// (`rewind cli --make-ico assets\rewind.ico`) when the drawing changes; the file is
/// embedded into the exe by build.rs.
pub fn write_ico(path: &Path) -> Result<()> {
    const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
    let images: Vec<Vec<u8>> = SIZES.iter().map(|&s| image_data(&app_pixels(s), s)).collect();
    let mut file = Vec::new();
    file.extend_from_slice(&[0, 0, 1, 0]);
    file.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (size, image) in SIZES.iter().zip(&images) {
        let side = if *size >= 256 { 0 } else { *size as u8 }; // 0 means 256
        file.extend_from_slice(&[side, side, 0, 0]);
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&(image.len() as u32).to_le_bytes());
        file.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in &images {
        file.extend_from_slice(image);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, file)?;
    Ok(())
}

/// Test aid: writes every icon, enlarged, on dark and light taskbar colours.
pub fn dump(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    const ZOOM: u32 = 8;
    for (light, bg) in [(false, [0x20u8, 0x20, 0x20]), (true, [0xF3, 0xF3, 0xF3])] {
        for size in [16u32, 24, 32] {
            let w = size * ZOOM * 3;
            let h = size * ZOOM;
            let row = (w * 3).div_ceil(4) * 4;
            let mut bmp = Vec::new();
            bmp.extend_from_slice(b"BM");
            bmp.extend_from_slice(&(54 + row * h).to_le_bytes());
            bmp.extend_from_slice(&[0u8; 4]);
            bmp.extend_from_slice(&54u32.to_le_bytes());
            bmp.extend_from_slice(&40u32.to_le_bytes());
            bmp.extend_from_slice(&(w as i32).to_le_bytes());
            bmp.extend_from_slice(&(h as i32).to_le_bytes());
            bmp.extend_from_slice(&1u16.to_le_bytes());
            bmp.extend_from_slice(&24u16.to_le_bytes());
            bmp.extend_from_slice(&[0u8; 24]);
            let icons: Vec<Vec<u8>> = ALL.iter().map(|s| pixels(*s, size, light)).collect();
            for y in (0..h).rev() {
                let mut line = Vec::with_capacity(row as usize);
                for x in 0..w {
                    let icon = &icons[(x / (size * ZOOM)) as usize];
                    let (ix, iy) = ((x % (size * ZOOM)) / ZOOM, y / ZOOM);
                    let i = ((iy * size + ix) * 4) as usize;
                    let a = icon[i + 3] as f32 / 255.0;
                    for c in 0..3 {
                        // BMP wants BGR; the background is given as RGB.
                        let under = bg[2 - c] as f32;
                        line.push((icon[i + c] as f32 * a + under * (1.0 - a)).round() as u8);
                    }
                }
                line.resize(row as usize, 0);
                bmp.extend_from_slice(&line);
            }
            let name = format!("icons-{}-{size}.bmp", if light { "light" } else { "dark" });
            std::fs::write(dir.join(name), bmp)?;
        }
    }
    Ok(())
}
