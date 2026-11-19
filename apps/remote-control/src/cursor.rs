//! Remote cursor shape. The host reads the cursor Windows shows (GetCursorInfo,
//! GetIconInfo) and sends each distinct image once under an id; after that a
//! repeat costs only the id. The viewer builds an HCURSOR from the same masks,
//! so colour, alpha and inverting (monochrome) cursors look as they do on the host.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use anyhow::{Result, bail};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::{
    Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetBitmapBits, GetDC, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
        ReleaseDC, SelectObject,
    },
    UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, CreateIconIndirect, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HCURSOR,
        HICON, ICONINFO,
    },
};

use crate::{log, pipeline::PipeShared, proto::Msg};

/// Largest cursor side sent (256 × 256 × 4 bytes is 256 KB on the control stream).
pub const MAX_SIDE: u16 = 256;
const POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// 32-bit BGRA rows, top-down; empty for a monochrome cursor.
    pub color: Vec<u8>,
    /// 1-bit AND mask rows, each padded to 16 bits as CreateBitmap wants.
    /// A monochrome cursor has its XOR mask rows after them (2 × height rows).
    pub mask: Vec<u8>,
}

impl CursorImage {
    pub fn stride(width: u16) -> usize {
        (width as usize).div_ceil(16) * 2
    }

    pub fn is_color(&self) -> bool {
        !self.color.is_empty()
    }

    pub fn mask_rows(&self) -> usize {
        if self.is_color() { self.height as usize } else { self.height as usize * 2 }
    }

    /// Sizes agree and are within limits (the image comes from the network).
    pub fn valid(&self) -> bool {
        let (w, h) = (self.width as usize, self.height as usize);
        (1..=MAX_SIDE as usize).contains(&w)
            && (1..=MAX_SIDE as usize).contains(&h)
            && self.hot_x < self.width
            && self.hot_y < self.height
            && (self.color.is_empty() || self.color.len() == w * h * 4)
            && self.mask.len() == Self::stride(self.width) * self.mask_rows()
    }

    fn mask_bit(&self, row: usize, x: usize) -> bool {
        self.mask[row * Self::stride(self.width) + x / 8] & (0x80 >> (x % 8)) != 0
    }

    /// Nearest-neighbour resize by `s`; the hotspot moves with the picture.
    pub fn scaled(&self, s: f64) -> CursorImage {
        let (w, h) = (self.width as usize, self.height as usize);
        let nw = ((w as f64 * s).round() as usize).clamp(1, MAX_SIDE as usize);
        let nh = ((h as f64 * s).round() as usize).clamp(1, MAX_SIDE as usize);
        if (nw, nh) == (w, h) {
            return self.clone();
        }
        let src = |i: usize, n: usize, old: usize| (i * old / n).min(old - 1);
        let stride = Self::stride(nw as u16);
        let planes = if self.is_color() { 1 } else { 2 };
        let mut mask = vec![0u8; stride * nh * planes];
        for p in 0..planes {
            for y in 0..nh {
                let sy = p * h + src(y, nh, h);
                for x in 0..nw {
                    if self.mask_bit(sy, src(x, nw, w)) {
                        mask[(p * nh + y) * stride + x / 8] |= 0x80 >> (x % 8);
                    }
                }
            }
        }
        let mut color = Vec::new();
        if self.is_color() {
            color.reserve(nw * nh * 4);
            for y in 0..nh {
                let sy = src(y, nh, h);
                for x in 0..nw {
                    let i = (sy * w + src(x, nw, w)) * 4;
                    color.extend_from_slice(&self.color[i..i + 4]);
                }
            }
        }
        CursorImage {
            width: nw as u16,
            height: nh as u16,
            hot_x: ((self.hot_x as f64 * nw as f64 / w as f64) as u16).min(nw as u16 - 1),
            hot_y: ((self.hot_y as f64 * nh as f64 / h as f64) as u16).min(nh as u16 - 1),
            color,
            mask,
        }
    }
}

struct Bitmaps(ICONINFO);

impl Drop for Bitmaps {
    fn drop(&mut self) {
        unsafe {
            if !self.0.hbmMask.is_invalid() {
                let _ = DeleteObject(self.0.hbmMask.into());
            }
            if !self.0.hbmColor.is_invalid() {
                let _ = DeleteObject(self.0.hbmColor.into());
            }
        }
    }
}

fn header(width: u16, height: u16) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The image of a cursor handle, or None if it cannot be read or is too large.
pub fn read(cursor: HCURSOR) -> Option<CursorImage> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(HICON(cursor.0), &mut ii).ok()?;
        let ii = Bitmaps(ii);
        let mut bm = BITMAP::default();
        if GetObjectW(ii.0.hbmMask.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut _)) == 0 {
            return None;
        }
        let color = !ii.0.hbmColor.is_invalid();
        let (w, mask_h) = (bm.bmWidth, bm.bmHeight);
        let h = if color { mask_h } else { mask_h / 2 };
        if w < 1 || h < 1 || w > MAX_SIDE as i32 || h > MAX_SIDE as i32 {
            return None;
        }
        let (w, h) = (w as u16, h as u16);
        let stride = CursorImage::stride(w);
        let mut raw = vec![0u8; bm.bmWidthBytes as usize * mask_h as usize];
        if GetBitmapBits(ii.0.hbmMask, raw.len() as i32, raw.as_mut_ptr() as *mut _) as usize != raw.len() {
            return None;
        }
        let mut mask = vec![0u8; stride * mask_h as usize];
        for (dst, src) in mask.chunks_exact_mut(stride).zip(raw.chunks_exact(bm.bmWidthBytes as usize)) {
            dst.copy_from_slice(&src[..stride]);
        }
        let mut bgra = Vec::new();
        if color {
            bgra = vec![0u8; w as usize * h as usize * 4];
            let dc = GetDC(None);
            let mut bi = header(w, h);
            let n = GetDIBits(dc, ii.0.hbmColor, 0, h as u32, Some(bgra.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
            ReleaseDC(None, dc);
            if n != h as i32 {
                return None;
            }
        }
        let img = CursorImage {
            width: w,
            height: h,
            hot_x: (ii.0.xHotspot as u16).min(w - 1),
            hot_y: (ii.0.yHotspot as u16).min(h - 1),
            color: bgra,
            mask,
        };
        img.valid().then_some(img)
    }
}

/// Builds a cursor from an image (viewer side). Destroy it with DestroyCursor.
pub fn build(img: &CursorImage) -> Result<HCURSOR> {
    if !img.valid() {
        bail!("bad cursor image");
    }
    unsafe {
        let mask = CreateBitmap(img.width as i32, img.mask_rows() as i32, 1, 1, Some(img.mask.as_ptr() as *const _));
        let mut color = HBITMAP::default();
        if img.is_color() {
            let mut bits = std::ptr::null_mut();
            color = CreateDIBSection(None, &header(img.width, img.height), DIB_RGB_COLORS, &mut bits, None, 0)?;
            std::ptr::copy_nonoverlapping(img.color.as_ptr(), bits as *mut u8, img.color.len());
        }
        let ii = ICONINFO {
            fIcon: false.into(),
            xHotspot: img.hot_x as u32,
            yHotspot: img.hot_y as u32,
            hbmMask: mask,
            hbmColor: color,
        };
        let r = CreateIconIndirect(&ii);
        let _ = DeleteObject(HGDIOBJ(mask.0));
        if !color.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        Ok(HCURSOR(r?.0))
    }
}

/// Draws a built cursor over white (left) and black (right) and saves it as
/// a PNG four times enlarged, so inverting pixels show up (for tests).
pub fn dump(cursor: HCURSOR, img: &CursorImage, path: &Path) -> Result<()> {
    let (w, h) = (img.width as usize, img.height as usize);
    let mut px = vec![0u8; w * 2 * h * 4];
    unsafe {
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(None, &header(img.width * 2, img.height), DIB_RGB_COLORS, &mut bits, None, 0)?;
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, dib.into());
        let buf = std::slice::from_raw_parts_mut(bits as *mut u32, w * 2 * h);
        for (i, p) in buf.iter_mut().enumerate() {
            *p = if i % (w * 2) < w { 0xFFFF_FFFF } else { 0xFF00_0000 };
        }
        let r = DrawIconEx(dc, 0, 0, HICON(cursor.0), w as i32, h as i32, 0, None, DI_NORMAL)
            .and_then(|_| DrawIconEx(dc, w as i32, 0, HICON(cursor.0), w as i32, h as i32, 0, None, DI_NORMAL));
        for (i, p) in buf.iter().enumerate() {
            let [b, g, r, _] = p.to_le_bytes();
            px[i * 4..i * 4 + 4].copy_from_slice(&[r, g, b, 255]);
        }
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(dib.into());
        r?;
    }
    const Z: usize = 4;
    let (zw, zh) = (w * 2 * Z, h * Z);
    let mut big = Vec::with_capacity(zw * zh * 4);
    for y in 0..zh {
        for x in 0..zw {
            let i = ((y / Z) * w * 2 + x / Z) * 4;
            big.extend_from_slice(&px[i..i + 4]);
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, zw as u32, zh as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&big)?;
    Ok(())
}

/// Host side, on its own thread until `sh.stop`: watches the cursor and
/// sends its shape when it changes. Scaled like the video (`PipeShared::scale`),
/// so the cursor keeps its size relative to the streamed picture.
pub fn watch(out: UnboundedSender<Msg>, sh: Arc<PipeShared>) {
    let mut ids: HashMap<u64, u32> = HashMap::new();
    let mut last: Option<(bool, isize, u32)> = None;
    let mut repeats = 0u32;
    let mut follow_failed = 0u64;
    while !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        // In service mode the cursor of the UAC prompt or lock screen is only
        // visible from a thread on that desktop.
        if let Err(e) = crate::desktop::follow() {
            follow_failed += 1;
            if follow_failed.is_power_of_two() {
                log!("cursor: cannot follow the input desktop ({follow_failed} times): {e:#}");
            }
        }
        let mut ci = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            continue;
        }
        let scale = *sh.scale.lock().unwrap();
        let scale = if scale > 0.0 && scale < 0.999 { scale } else { 1.0 };
        let showing = ci.flags.0 & CURSOR_SHOWING.0 != 0 && !ci.hCursor.is_invalid();
        let key = (showing, if showing { ci.hCursor.0 as isize } else { 0 }, (scale * 1000.0).round() as u32);
        if last == Some(key) {
            continue;
        }
        last = Some(key);
        let msg = if !showing {
            Msg::CursorHidden
        } else {
            let Some(mut img) = read(ci.hCursor) else { continue };
            if scale < 1.0 {
                img = img.scaled(scale);
            }
            let mut hasher = DefaultHasher::new();
            img.hash(&mut hasher);
            let next = ids.len() as u32 + 1;
            match ids.entry(hasher.finish()) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    repeats += 1;
                    Msg::CursorUse { id: *e.get() }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(next);
                    Msg::CursorShape { id: next, image: img }
                }
            }
        };
        if out.send(msg).is_err() {
            break;
        }
    }
    log!("cursor watch stopped: {} shapes sent, {repeats} repeats sent as ids", ids.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(w: u16, h: u16) -> CursorImage {
        let stride = CursorImage::stride(w);
        let mut mask = vec![0u8; stride * h as usize * 2];
        // AND: left column transparent; XOR: top row inverted.
        for y in 0..h as usize {
            mask[y * stride] |= 0x80;
        }
        for x in 0..w as usize {
            mask[h as usize * stride + x / 8] |= 0x80 >> (x % 8);
        }
        CursorImage { width: w, height: h, hot_x: 1, hot_y: 2, color: vec![], mask }
    }

    #[test]
    fn system_cursors_read_build_and_draw() {
        use windows::Win32::UI::WindowsAndMessaging::{DestroyCursor, IDC_ARROW, IDC_HAND, IDC_IBEAM, LoadCursorW};
        let dir = std::env::temp_dir().join(format!("rc-cursor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, id) in [("arrow", IDC_ARROW), ("ibeam", IDC_IBEAM), ("hand", IDC_HAND)] {
            let h = unsafe { LoadCursorW(None, id) }.unwrap();
            let img = read(h).unwrap_or_else(|| panic!("{name}: not readable"));
            assert!(img.valid(), "{name}");
            let built = build(&img.scaled(0.75)).unwrap();
            let path = dir.join(format!("{name}.png"));
            dump(built, &img.scaled(0.75), &path).unwrap();
            assert!(std::fs::metadata(&path).unwrap().len() > 0);
            unsafe { DestroyCursor(built) }.unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strides_are_word_aligned() {
        assert_eq!(CursorImage::stride(1), 2);
        assert_eq!(CursorImage::stride(16), 2);
        assert_eq!(CursorImage::stride(17), 4);
        assert_eq!(CursorImage::stride(32), 4);
        assert!(mono(32, 32).valid());
        assert!(mono(17, 5).valid());
    }

    #[test]
    fn invalid_images_are_refused() {
        let mut m = mono(32, 32);
        m.mask.pop();
        assert!(!m.valid());
        let mut m = mono(32, 32);
        m.hot_x = 32;
        assert!(!m.valid());
        let mut m = mono(32, 32);
        m.color = vec![0; 10];
        assert!(!m.valid());
        let mut big = mono(MAX_SIDE, 1);
        big.width = MAX_SIDE + 1;
        assert!(!big.valid());
    }

    #[test]
    fn scaling_keeps_both_masks_and_hotspot() {
        let m = mono(32, 32).scaled(1.5);
        assert_eq!((m.width, m.height, m.hot_x, m.hot_y), (48, 48, 1, 3));
        assert!(m.valid());
        for y in 0..48 {
            assert!(m.mask_bit(y, 0), "AND column at row {y}");
            assert!(!m.mask_bit(y, 2));
        }
        for x in 0..48 {
            assert!(m.mask_bit(48, x), "XOR row at column {x}");
            assert!(!m.mask_bit(50, x));
        }
        assert_eq!(mono(32, 32).scaled(1.0), mono(32, 32));
    }

    #[test]
    fn scaling_a_colour_cursor() {
        let (w, h) = (4u16, 2u16);
        let color: Vec<u8> = (0..(w * h) as u8).flat_map(|i| [i, i, i, 255]).collect();
        let img = CursorImage {
            width: w,
            height: h,
            hot_x: 3,
            hot_y: 1,
            color,
            mask: vec![0u8; CursorImage::stride(w) * h as usize],
        };
        let s = img.scaled(0.5);
        assert_eq!((s.width, s.height, s.hot_x, s.hot_y), (2, 1, 1, 0));
        assert_eq!(s.color, vec![0, 0, 0, 255, 2, 2, 2, 255]);
        assert!(s.valid());
    }
}
