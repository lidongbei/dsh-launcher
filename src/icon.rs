//! The tray artwork: the DeepSeek tile in its brand blue, with the service
//! state on a small badge in the corner.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, GetSystemMetrics, HICON, ICONINFO, SM_CXSMICON,
};

/// The 96px master and the 16px frame that the DeepSeek Harness build itself
/// uses for the notification area.
const LOGO_BIG: &[u8] = include_bytes!("../assets/logo96.png");
const LOGO_SMALL: &[u8] = include_bytes!("../assets/logo16.png");

/// DeepSeek's whale blue.
pub const BRAND: (u8, u8, u8) = (78, 108, 254);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Stopped,
    Working,
    Unknown,
}

impl State {
    pub fn color(self) -> (u8, u8, u8) {
        match self {
            State::Running => (34, 197, 94),
            State::Stopped => (239, 68, 68),
            State::Working => (245, 158, 11),
            State::Unknown => (107, 114, 128),
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Stopped => "stopped",
            State::Working => "working",
            State::Unknown => "unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Running => "运行中",
            State::Stopped => "已停止",
            State::Working => "操作中",
            State::Unknown => "状态未知",
        }
    }
}

/// RGBA8 image, top-down.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

fn decode_png(bytes: &[u8]) -> Result<Image, String> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("cannot read icon png: {e}"))?;
    let width = reader.info().width;
    let height = reader.info().height;
    let mut buffer = vec![0u8; width as usize * height as usize * 4];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|e| format!("cannot decode icon png: {e}"))?;
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return Err("icon png must be 8-bit RGBA".to_string());
    }
    Ok(Image {
        width,
        height,
        pixels: buffer,
    })
}

fn scaled(source: &Image, size: u32) -> Image {
    if source.width == size && source.height == size {
        return Image {
            width: size,
            height: size,
            pixels: source.pixels.clone(),
        };
    }
    let mut pixels = vec![0u8; size as usize * size as usize * 4];
    let scale_x = source.width as f32 / size as f32;
    let scale_y = source.height as f32 / size as f32;
    for y in 0..size {
        for x in 0..size {
            // box filter: average every source pixel that falls into this cell
            let x0 = (x as f32 * scale_x).floor() as u32;
            let x1 = (((x + 1) as f32) * scale_x).ceil() as u32;
            let y0 = (y as f32 * scale_y).floor() as u32;
            let y1 = (((y + 1) as f32) * scale_y).ceil() as u32;
            let (mut r, mut g, mut b, mut a, mut count) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1.min(source.height) {
                for sx in x0..x1.min(source.width) {
                    let index = ((sy * source.width + sx) * 4) as usize;
                    let alpha = source.pixels[index + 3] as u32;
                    r += source.pixels[index] as u32 * alpha;
                    g += source.pixels[index + 1] as u32 * alpha;
                    b += source.pixels[index + 2] as u32 * alpha;
                    a += alpha;
                    count += 1;
                }
            }
            let index = ((y * size + x) * 4) as usize;
            if count == 0 || a == 0 {
                continue;
            }
            pixels[index] = (r / a) as u8;
            pixels[index + 1] = (g / a) as u8;
            pixels[index + 2] = (b / a) as u8;
            pixels[index + 3] = (a / count) as u8;
        }
    }
    Image {
        width: size,
        height: size,
        pixels,
    }
}

/// Recolours the dark glyph into the brand blue, leaving the white tile alone.
fn tint_brand(image: &mut Image) {
    for pixel in image.pixels.chunks_exact_mut(4) {
        let alpha = pixel[3];
        if alpha == 0 {
            continue;
        }
        let luminance =
            0.299 * pixel[0] as f32 + 0.587 * pixel[1] as f32 + 0.114 * pixel[2] as f32;
        let mut ink = (255.0 - luminance) / 207.0;
        if ink <= 0.02 {
            continue;
        }
        ink = ink.min(1.0);
        for channel in 0..3 {
            let target = [BRAND.0, BRAND.1, BRAND.2][channel] as f32;
            pixel[channel] = (255.0 + (target - 255.0) * ink).round() as u8;
        }
    }
}

fn blend_circle(image: &mut Image, centre: f32, radius: f32, color: (u8, u8, u8)) {
    let size = image.width as f32;
    for y in 0..image.height {
        for x in 0..image.width {
            let dx = (x as f32 + 0.5) - centre;
            let dy = (y as f32 + 0.5) - centre;
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let _ = size;
            let index = ((y * image.width + x) * 4) as usize;
            let alpha = image.pixels[index + 3] as f32 / 255.0;
            let over = coverage;
            let out_alpha = over + alpha * (1.0 - over);
            if out_alpha <= 0.0 {
                continue;
            }
            for channel in 0..3 {
                let back = image.pixels[index + channel] as f32;
                let front = [color.0, color.1, color.2][channel] as f32;
                let value = (front * over + back * alpha * (1.0 - over)) / out_alpha;
                image.pixels[index + channel] = value.round().clamp(0.0, 255.0) as u8;
            }
            image.pixels[index + 3] = (out_alpha * 255.0).round() as u8;
        }
    }
}

/// The tile at `size` pixels, brand blue, with the state badge bottom-right.
pub fn render(state: State, size: u32) -> Result<Image, String> {
    let size = size.max(8);
    let source = if size <= 24 {
        decode_png(LOGO_SMALL)?
    } else {
        decode_png(LOGO_BIG)?
    };
    let mut image = scaled(&source, size);
    tint_brand(&mut image);

    // Deliberately generous: at 16px a ~6px dot reads much better than a 4px
    // one, and the margin keeps the white ring off the tile edge.
    let radius = ((size as f32) * 0.165).max(1.9);
    let ring = ((size as f32) * 0.045).max(0.7);
    let margin = ((size as f32) * 0.0375).max(0.6);
    let centre = size as f32 - margin - ring - radius;
    blend_circle(&mut image, centre, radius + ring, (255, 255, 255));
    blend_circle(&mut image, centre, radius, state.color());
    Ok(image)
}

fn to_hicon(image: &Image) -> Result<HICON, String> {
    let width = image.width as i32;
    let height = image.height as i32;

    let header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // negative height keeps the rows top-down
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    unsafe {
        let screen = GetDC(None);
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(Some(screen), &header, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| format!("CreateDIBSection failed: {e}"))?;
        let raw = std::slice::from_raw_parts_mut(
            bits as *mut u8,
            image.width as usize * image.height as usize * 4,
        );
        for (source, target) in image.pixels.chunks_exact(4).zip(raw.chunks_exact_mut(4)) {
            target[0] = source[2];
            target[1] = source[1];
            target[2] = source[0];
            target[3] = source[3];
        }

        let mask_bits = vec![0u8; ((width + 15) / 16 * 2) as usize * height as usize];
        let mask = CreateBitmap(width, height, 1, 1, Some(mask_bits.as_ptr() as *const _));

        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(HGDIOBJ(mask.0));
        let _ = DeleteObject(HGDIOBJ(color.0));
        ReleaseDC(None, screen);
        icon.map_err(|e| format!("CreateIconIndirect failed: {e}"))
    }
}

/// Four icons at the size the shell actually draws them.
pub fn build_set(size: u32) -> Result<Vec<(State, HICON)>, String> {
    let mut set = Vec::new();
    for state in [State::Running, State::Stopped, State::Working, State::Unknown] {
        let image = render(state, size)?;
        set.push((state, to_hicon(&image)?));
    }
    Ok(set)
}

pub fn icon_size() -> u32 {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    size.max(16) as u32
}

pub fn destroy(icon: HICON) {
    unsafe {
        let _ = DestroyIcon(icon);
    }
}

/// Writes one PNG per state and size, so the artwork can be checked without
/// a notification area in sight.
pub fn dump_states(dir: &Path, sizes: &[u32]) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut written = Vec::new();
    for size in sizes {
        for state in [State::Running, State::Stopped, State::Working, State::Unknown] {
            let image = render(state, *size)?;
            let path = dir.join(format!("state-{}-{size}.png", state.slug()));
            let file = File::create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            let mut encoder = png::Encoder::new(BufWriter::new(file), image.width, image.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            writer
                .write_image_data(&image.pixels)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            written.push(path);
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * image.width + x) * 4) as usize;
        [
            image.pixels[offset],
            image.pixels[offset + 1],
            image.pixels[offset + 2],
            image.pixels[offset + 3],
        ]
    }

    fn badge_color(state: State) -> [u8; 4] {
        let color = match state {
            State::Running => (34, 197, 94),
            State::Stopped => (239, 68, 68),
            State::Working => (245, 158, 11),
            State::Unknown => (107, 114, 128),
        };
        [color.0, color.1, color.2, 255]
    }

    #[test]
    fn corners_stay_transparent() {
        for state in [State::Running, State::Stopped, State::Working, State::Unknown] {
            for size in [16, 32, 48] {
                let image = render(state, size).expect("render should succeed");
                assert_eq!(image.width, size);
                assert_eq!(pixel(&image, 0, 0)[3], 0, "top-left corner should be clear");
                assert_eq!(pixel(&image, size - 1, 0)[3], 0, "top-right corner should be clear");
                assert_eq!(pixel(&image, 0, size - 1)[3], 0, "bottom-left should be clear");
            }
        }
    }

    #[test]
    fn badge_carries_the_state_colour() {
        for state in [State::Running, State::Stopped, State::Working, State::Unknown] {
            for size in [16, 32, 48] {
                let image = render(state, size).expect("render should succeed");
                let expected = badge_color(state);
                let mut matches = 0;
                for x in (size * 6 / 10)..size {
                    for y in (size * 6 / 10)..size {
                        if pixel(&image, x, y) == expected {
                            matches += 1;
                        }
                    }
                }
                assert!(
                    matches >= (size as usize / 4),
                    "badge for {state:?} at {size}px should cover the corner, found {matches} pixels"
                );
            }
        }
    }

    #[test]
    fn whale_keeps_the_brand_blue() {
        let image = render(State::Running, 48).expect("render should succeed");
        let center = pixel(&image, 24, 24);
        assert_eq!(center[3], 255, "whale body should be opaque");
        assert!(center[2] > 200, "whale should be blue, got {center:?}");
    }
}
