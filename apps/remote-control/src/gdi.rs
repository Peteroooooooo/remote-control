//! Drawing for the small Win32 windows the host shows (the confirmation
//! popup, the "being controlled" bar): a back buffer, fonts in pixels at the
//! window's scaling, smooth (anti-aliased) rounded rectangles and circles,
//! text, and layered windows that are see-through outside their shape.
//! Colours come from `theme`, the lobby's palette.

use std::ptr;

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, POINT, RECT, SIZE},
        Graphics::Gdi::{
            AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, BeginPaint, BitBlt,
            CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush,
            DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC,
            DeleteObject, DrawTextW, EndPaint, FillRect, GdiFlush, GetStockObject, GetTextExtentPoint32W, HDC, HFONT,
            HGDIOBJ, NULL_BRUSH, NULL_PEN, PAINTSTRUCT, PS_INSIDEFRAME, RoundRect, SRCCOPY, SelectObject, SetBkMode,
            SetTextColor, TRANSPARENT,
        },
        UI::{
            HiDpi::GetDpiForWindow,
            WindowsAndMessaging::{GetClientRect, ULW_ALPHA, UpdateLayeredWindow},
        },
    },
    core::w,
};

use crate::theme;

/// 0x00RRGGBB as GDI wants it (0x00BBGGRR).
pub const fn rgb(c: u32) -> COLORREF {
    COLORREF(((c & 0xff) << 16) | (c & 0xff00) | ((c >> 16) & 0xff))
}

pub const BG: COLORREF = rgb(theme::CHROME);
pub const TEXT: COLORREF = rgb(theme::TEXT);
pub const DIM: COLORREF = rgb(theme::DIM);
pub const ACCENT: COLORREF = rgb(theme::ACCENT);
pub const ACCENT_HOVER: COLORREF = rgb(theme::ACCENT_HOVER);
pub const BUTTON_EDGE: COLORREF = rgb(theme::BUTTON_EDGE);
pub const DANGER_BTN: COLORREF = rgb(theme::DANGER_BTN);
pub const DANGER_BTN_HOVER: COLORREF = rgb(theme::DANGER_BTN_HOVER);

/// Window units (96 DPI) to pixels.
pub fn scale(hwnd: HWND) -> impl Fn(i32) -> i32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as i32;
    move |v| v * dpi / 96
}

pub fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

pub fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// Paints into a back buffer the size of the client area and copies it to
/// the window when dropped (no flicker). An off-screen one (`offscreen`)
/// goes to a layered window instead (`present_layered`).
pub struct Canvas {
    /// The window being painted (`begin`): copied there when dropped.
    paint: Option<(HWND, PAINTSTRUCT, HDC)>,
    pub dc: HDC,
    bmp: HGDIOBJ,
    old_bmp: HGDIOBJ,
    pub size: RECT,
    fonts: Vec<HFONT>,
    /// The back buffer's pixels (0x00RRGGBB, rows top-down), for the smooth
    /// shapes; null if the buffer is a plain bitmap.
    bits: *mut u32,
}

impl Canvas {
    pub fn begin(hwnd: HWND, bg: COLORREF) -> Canvas {
        Canvas::begin_top(hwnd, bg, i32::MAX)
    }

    /// The same for only the top `height` pixels of the client area.
    pub fn begin_top(hwnd: HWND, bg: COLORREF, height: i32) -> Canvas {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let win = BeginPaint(hwnd, &mut ps);
            let mut size = RECT::default();
            let _ = GetClientRect(hwnd, &mut size);
            size.bottom = size.bottom.min(height);
            let mut c = Canvas::buffer(Some(win), size);
            c.paint = Some((hwnd, ps, win));
            c.fill(&size, bg);
            c
        }
    }

    /// A buffer `w` × `h` pixels not tied to a window, filled with `bg`.
    pub fn offscreen(w: i32, h: i32, bg: COLORREF) -> Canvas {
        let size = RECT { left: 0, top: 0, right: w, bottom: h };
        let c = Canvas::buffer(None, size);
        c.fill(&size, bg);
        c
    }

    fn buffer(win: Option<HDC>, size: RECT) -> Canvas {
        unsafe {
            let (w, h) = (size.right.max(1), size.bottom.max(1));
            let dc = CreateCompatibleDC(win);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = ptr::null_mut();
            let (bmp, bits) = match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) if !bits.is_null() => (HGDIOBJ(b.0), bits as *mut u32),
                _ => (HGDIOBJ(CreateCompatibleBitmap(win.unwrap_or(dc), w, h).0), ptr::null_mut()),
            };
            let old_bmp = SelectObject(dc, bmp);
            SetBkMode(dc, TRANSPARENT);
            Canvas { paint: None, dc, bmp, old_bmp, size, fonts: Vec::new(), bits }
        }
    }

    /// How wide `s` is in `font`, in pixels.
    pub fn text_width(&self, s: &str, font: HFONT) -> i32 {
        unsafe {
            let old = SelectObject(self.dc, HGDIOBJ(font.0));
            let buf: Vec<u16> = s.encode_utf16().collect();
            let mut sz = SIZE::default();
            let _ = GetTextExtentPoint32W(self.dc, &buf, &mut sz);
            SelectObject(self.dc, old);
            sz.cx
        }
    }

    /// Shows the buffer as the layered window `hwnd` at (`x`, `y`) on the
    /// screen, each pixel as opaque as `alpha(x, y)` (0..=1, at the pixel's
    /// centre) says; fully clear pixels let the mouse through.
    pub fn present_layered(&self, hwnd: HWND, x: i32, y: i32, alpha: impl Fn(f32, f32) -> f32) {
        if self.bits.is_null() {
            return;
        }
        let (w, h) = (self.size.right.max(1), self.size.bottom.max(1));
        unsafe {
            let _ = GdiFlush();
            for py in 0..h {
                for px in 0..w {
                    let p = &mut *self.bits.add((py * w + px) as usize);
                    let a = (alpha(px as f32 + 0.5, py as f32 + 0.5).clamp(0.0, 1.0) * 255.0).round() as u32;
                    // Premultiplied, as UpdateLayeredWindow wants it.
                    let v = *p;
                    let ch = |s: u32| ((v >> s & 0xff) * a + 127) / 255;
                    *p = (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0);
                }
            }
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let (pos, size, src) = (POINT { x, y }, SIZE { cx: w, cy: h }, POINT::default());
            let _ = UpdateLayeredWindow(hwnd, None, Some(&pos), Some(&size), Some(self.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }

    /// A Microsoft YaHei UI font `px` pixels high; freed with the canvas.
    pub fn font(&mut self, px: i32, bold: bool) -> HFONT {
        let f = unsafe {
            CreateFontW(
                -px,
                0,
                0,
                0,
                if bold { 700 } else { 400 },
                0,
                0,
                0,
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                0,
                w!("Microsoft YaHei UI"),
            )
        };
        self.fonts.push(f);
        f
    }

    pub fn fill(&self, r: &RECT, color: COLORREF) {
        unsafe {
            let b = CreateSolidBrush(color);
            FillRect(self.dc, r, b);
            let _ = DeleteObject(HGDIOBJ(b.0));
        }
    }

    /// A rounded rectangle, filled and/or with an outline `(color, width)`
    /// inside its edge.
    pub fn round(&self, r: &RECT, radius: i32, fill: Option<COLORREF>, outline: Option<(COLORREF, i32)>) {
        let [l, t, rr, b] = [r.left, r.top, r.right, r.bottom].map(|v| v as f32);
        self.shape([l, t, rr, b], radius as f32, fill, outline);
    }

    /// A circle covering the pixels `cx - radius ..= cx + radius`.
    pub fn circle(&self, cx: i32, cy: i32, radius: i32, fill: Option<COLORREF>, outline: Option<(COLORREF, i32)>) {
        let [x, y, r] = [cx, cy, radius].map(|v| v as f32);
        self.shape([x - r, y - r, x + r + 1.0, y + r + 1.0], r + 0.5, fill, outline);
    }

    /// A smooth line `width` pixels wide with round ends, through the points
    /// in turn (pixel coordinates, fractions allowed).
    pub fn lines(&self, points: &[(f32, f32)], width: f32, color: COLORREF) {
        if self.bits.is_null() || points.len() < 2 {
            return;
        }
        let _ = unsafe { GdiFlush() };
        let (w, h) = (self.size.right.max(1), self.size.bottom.max(1));
        let half = width / 2.0;
        let xs = points.iter().map(|p| p.0);
        let ys = points.iter().map(|p| p.1);
        let (l, r) = (xs.clone().fold(f32::MAX, f32::min) - half - 1.0, xs.fold(f32::MIN, f32::max) + half + 1.0);
        let (t, b) = (ys.clone().fold(f32::MAX, f32::min) - half - 1.0, ys.fold(f32::MIN, f32::max) + half + 1.0);
        for y in (t.floor() as i32).max(0)..(b.ceil() as i32).min(h) {
            for x in (l.floor() as i32).max(0)..(r.ceil() as i32).min(w) {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                // The nearest segment decides, so joints are not drawn twice.
                let d = points.windows(2).map(|s| segment_distance(s[0], s[1], px, py)).fold(f32::MAX, f32::min);
                let a = (half + 0.5 - d).clamp(0.0, 1.0);
                if a > 0.0 {
                    // Safety: x and y are inside the buffer, which is w x h.
                    let p = unsafe { &mut *self.bits.add((y * w + x) as usize) };
                    *p = blend(*p, color, a);
                }
            }
        }
    }

    /// Blends a rounded rectangle (`[left, top, right, bottom]` in pixels)
    /// into the back buffer, its edge smoothed over one pixel.
    fn shape(&self, [l, t, r, b]: [f32; 4], radius: f32, fill: Option<COLORREF>, outline: Option<(COLORREF, i32)>) {
        if self.bits.is_null() {
            self.gdi_shape([l, t, r, b].map(|v| v as i32), radius as i32, fill, outline);
            return;
        }
        // Let GDI finish what it has drawn so far before writing pixels.
        let _ = unsafe { GdiFlush() };
        let (w, h) = (self.size.right.max(1), self.size.bottom.max(1));
        let (x0, x1) = ((l.floor() as i32).max(0), (r.ceil() as i32).min(w));
        let (y0, y1) = ((t.floor() as i32).max(0), (b.ceil() as i32).min(h));
        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let outer = coverage([l, t, r, b], radius, px, py);
                if outer <= 0.0 {
                    continue;
                }
                // Safety: x and y are inside the buffer, which is w x h.
                let p = unsafe { &mut *self.bits.add((y * w + x) as usize) };
                if let Some(c) = fill {
                    *p = blend(*p, c, outer);
                }
                if let Some((c, width)) = outline {
                    let s = width as f32;
                    let inner = coverage([l + s, t + s, r - s, b - s], (radius - s).max(0.0), px, py);
                    *p = blend(*p, c, (outer - inner).max(0.0));
                }
            }
        }
    }

    /// The same with GDI's own (jagged) rounded rectangle, for a back buffer
    /// without pixel access.
    fn gdi_shape(&self, [l, t, r, b]: [i32; 4], radius: i32, fill: Option<COLORREF>, outline: Option<(COLORREF, i32)>) {
        unsafe {
            let brush = fill.map(|c| CreateSolidBrush(c));
            let pen = outline.map(|(c, w)| CreatePen(PS_INSIDEFRAME, w, c));
            let ob = SelectObject(self.dc, brush.map_or(GetStockObject(NULL_BRUSH), |b| HGDIOBJ(b.0)));
            let op = SelectObject(self.dc, pen.map_or(GetStockObject(NULL_PEN), |p| HGDIOBJ(p.0)));
            let _ = RoundRect(self.dc, l, t, r, b, radius * 2, radius * 2);
            SelectObject(self.dc, ob);
            SelectObject(self.dc, op);
            if let Some(b) = brush {
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
            if let Some(p) = pen {
                let _ = DeleteObject(HGDIOBJ(p.0));
            }
        }
    }

    /// One line in `r`, vertically centred; `center` for horizontally too.
    pub fn text(&self, r: &RECT, s: &str, font: HFONT, color: COLORREF, center: bool) {
        unsafe {
            let old = SelectObject(self.dc, HGDIOBJ(font.0));
            SetTextColor(self.dc, color);
            let mut buf: Vec<u16> = s.encode_utf16().collect();
            let mut r = *r;
            let h = if center { DT_CENTER } else { DT_LEFT };
            DrawTextW(self.dc, &mut buf, &mut r, h | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            SelectObject(self.dc, old);
        }
    }
}

/// How much of the pixel centred at (x, y) a rounded rectangle covers, 0..=1.
pub fn coverage([l, t, r, b]: [f32; 4], radius: f32, x: f32, y: f32) -> f32 {
    let (hw, hh) = ((r - l) / 2.0, (b - t) / 2.0);
    if hw <= 0.0 || hh <= 0.0 {
        return 0.0;
    }
    let radius = radius.clamp(0.0, hw.min(hh));
    let qx = (x - (l + r) / 2.0).abs() - (hw - radius);
    let qy = (y - (t + b) / 2.0).abs() - (hh - radius);
    let distance = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
    (0.5 - distance).clamp(0.0, 1.0)
}

fn segment_distance((ax, ay): (f32, f32), (bx, by): (f32, f32), x: f32, y: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len = dx * dx + dy * dy;
    let k = if len > 0.0 { (((x - ax) * dx + (y - ay) * dy) / len).clamp(0.0, 1.0) } else { 0.0 };
    (x - ax - k * dx).hypot(y - ay - k * dy)
}

/// `color` over the buffer pixel `dst` (0x00RRGGBB) with opacity `a`.
fn blend(dst: u32, color: COLORREF, a: f32) -> u32 {
    let c = color.0;
    let src = [c & 0xff, (c >> 8) & 0xff, (c >> 16) & 0xff];
    let old = [(dst >> 16) & 0xff, (dst >> 8) & 0xff, dst & 0xff];
    let mix = |s: u32, d: u32| (d as f32 + (s as f32 - d as f32) * a).round() as u32;
    (mix(src[0], old[0]) << 16) | (mix(src[1], old[1]) << 8) | mix(src[2], old[2])
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            let s = self.size;
            if let Some((_, _, win)) = &self.paint {
                let _ = BitBlt(*win, 0, 0, s.right, s.bottom, Some(self.dc), 0, 0, SRCCOPY);
            }
            SelectObject(self.dc, self.old_bmp);
            let _ = DeleteObject(self.bmp);
            let _ = DeleteDC(self.dc);
            for f in self.fonts.drain(..) {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
            if let Some((hwnd, ps, _)) = &self.paint {
                let _ = EndPaint(*hwnd, ps);
            }
        }
    }
}
