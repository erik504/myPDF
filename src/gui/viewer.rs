//! Visualizzatore: pagine una sotto l'altra, zoom, scorrimento e disegno a riquadri.
//!
//! Le pagine piccole vengono disegnate intere; quelle grandi (zoom forte) a riquadri di
//! [`TILE`] pixel, così si chiede al motore solo la parte visibile. Finché un riquadro non è
//! pronto si mostra, ingrandito o rimpicciolito, quello della scala precedente.

use super::worker::{TileKey, TileReq, Worker};
use crate::render::Bitmap;
use std::collections::{HashMap, HashSet};
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW, LOGFONTW, CLEARTYPE_QUALITY, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, FillRect,
    SelectObject, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, COLORONCOLOR, DIB_RGB_COLORS,
    DT_CALCRECT, DT_CENTER, DT_NOPREFIX, DT_WORDBREAK, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Controls::SetScrollInfo;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, SCROLLINFO, SB_HORZ, SB_VERT, SIF_DISABLENOSCROLL, SIF_PAGE, SIF_POS, SIF_RANGE};

/// Pagine fino a questi pixel si disegnano in un colpo solo.
const SINGLE_TILE_PIXELS: i64 = 6_000_000;
/// Lato dei riquadri per le pagine più grandi.
pub const TILE: i32 = 1024;
/// Memoria massima per le immagini delle pagine già disegnate.
const CACHE_BUDGET: usize = 384 * 1024 * 1024;
pub const MIN_ZOOM: f64 = 0.05;
pub const MAX_ZOOM: f64 = 16.0;
/// Passi dei pulsanti di zoom.
const ZOOM_STEPS: [f64; 24] =
    [0.05, 0.1, 0.125, 0.25, 0.333, 0.5, 0.667, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 16.0];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoomMode {
    FitWidth,
    FitPage,
    Custom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PageRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Un punto del documento: pagina e posizione relativa al suo interno, tenuto fermo nella
/// finestra (in `cx`, `cy`) quando cambiano zoom o dimensioni.
#[derive(Clone, Copy, Debug)]
pub struct Anchor {
    pub page: usize,
    pub fx: f64,
    pub fy: f64,
    pub cx: i32,
    pub cy: i32,
}

/// Vista salvata di un documento, per ritrovarla tornando su di esso.
#[derive(Clone, Copy, Debug)]
pub struct SavedView {
    pub mode: ZoomMode,
    pub zoom: f64,
    pub page: usize,
    pub fy: f64,
}

struct Tile {
    bmp: HBITMAP,
    w: i32,
    h: i32,
    /// Posizione del riquadro nella pagina e dimensioni della pagina, alla scala del riquadro.
    ox: i32,
    oy: i32,
    pw: i32,
    ph: i32,
    used: u64,
}

impl Tile {
    fn bytes(&self) -> usize {
        self.w as usize * self.h as usize * 4
    }
}

#[derive(Default)]
struct TileCache {
    map: HashMap<TileKey, Tile>,
    bytes: usize,
}

impl TileCache {
    fn insert(&mut self, key: TileKey, tile: Tile) {
        self.bytes += tile.bytes();
        if let Some(old) = self.map.insert(key, tile) {
            self.bytes -= old.bytes();
            unsafe {
                let _ = DeleteObject(old.bmp);
            }
        }
    }
    fn remove_where(&mut self, mut f: impl FnMut(&TileKey, &Tile) -> bool) {
        let keys: Vec<TileKey> = self.map.iter().filter(|(k, t)| f(k, t)).map(|(k, _)| *k).collect();
        for k in keys {
            if let Some(t) = self.map.remove(&k) {
                self.bytes -= t.bytes();
                unsafe {
                    let _ = DeleteObject(t.bmp);
                }
            }
        }
    }
    /// Libera i riquadri usati meno di recente finché si rientra nel limite (mai quelli
    /// mostrati nell'ultimo disegno).
    fn trim(&mut self, frame: u64) {
        if self.bytes <= CACHE_BUDGET {
            return;
        }
        let mut entries: Vec<(u64, TileKey, usize)> = self.map.iter().filter(|(_, t)| t.used < frame).map(|(k, t)| (t.used, *k, t.bytes())).collect();
        entries.sort_by_key(|e| e.0);
        let mut over = self.bytes - CACHE_BUDGET;
        let mut drop = Vec::new();
        for (_, k, b) in entries {
            if over == 0 {
                break;
            }
            drop.push(k);
            over = over.saturating_sub(b);
        }
        self.remove_where(|k, _| drop.contains(k));
    }
}

impl Drop for TileCache {
    fn drop(&mut self) {
        self.remove_where(|_, _| true);
    }
}

pub struct Viewer {
    pub hwnd: HWND,
    pub doc: Option<u64>,
    sizes: Vec<(f32, f32)>,
    /// File danneggiato: dopo le pagine recuperate c'è la pagina "Broken PDF format".
    damaged: bool,
    /// Testo mostrato al posto delle pagine (nessun documento, errore, caricamento…).
    pub message: Option<String>,
    pub mode: ZoomMode,
    pub zoom: f64,
    pub dpi: u32,
    scale: f64,
    pages: Vec<PageRect>,
    content_w: i32,
    content_h: i32,
    pub sx: i32,
    pub sy: i32,
    pub cw: i32,
    pub ch: i32,
    cache: TileCache,
    /// Riquadri che il motore non è riuscito a disegnare: non si richiedono di nuovo.
    failed: HashSet<TileKey>,
    frame: u64,
    /// Trascinamento in corso: (x, y iniziali del mouse, scorrimento iniziale).
    pub drag: Option<(i32, i32, i32, i32)>,
    backbuf: Option<(HBITMAP, i32, i32)>,
    bg: HBRUSH,
    shadow: HBRUSH,
    white: HBRUSH,
    pub font: HFONT,
}

fn quantize_scale(s: f64) -> f64 {
    ((s * 4096.0).round() / 4096.0).max(1.0 / 4096.0)
}

fn scale_key(s: f64) -> u32 {
    (s * 4096.0).round() as u32
}

/// Dimensioni in pixel di una pagina di `size` DIP alla scala `scale`.
fn page_px(size: (f32, f32), scale: f64) -> (i32, i32) {
    (((size.0 as f64 * scale).round() as i32).max(1), ((size.1 as f64 * scale).round() as i32).max(1))
}

/// Lato dei riquadri di una pagina di `w` × `h` pixel.
fn tile_dims(w: i32, h: i32) -> (i32, i32) {
    if (w as i64) * (h as i64) <= SINGLE_TILE_PIXELS {
        (w, h)
    } else {
        (TILE, TILE)
    }
}

impl Viewer {
    pub fn new(hwnd: HWND, dpi: u32) -> Viewer {
        unsafe {
            Viewer {
                hwnd,
                doc: None,
                sizes: Vec::new(),
                damaged: false,
                message: None,
                mode: ZoomMode::FitWidth,
                zoom: 1.0,
                dpi,
                scale: 1.0,
                pages: Vec::new(),
                content_w: 0,
                content_h: 0,
                sx: 0,
                sy: 0,
                cw: 0,
                ch: 0,
                cache: TileCache::default(),
                failed: HashSet::new(),
                frame: 0,
                drag: None,
                backbuf: None,
                bg: CreateSolidBrush(COLORREF(0x0059_5652)),
                shadow: CreateSolidBrush(COLORREF(0x0033_3130)),
                white: CreateSolidBrush(COLORREF(0x00FF_FFFF)),
                font: HFONT::default(),
            }
        }
    }

    /// Pagine vere del documento (senza la pagina finale "Broken PDF format").
    pub fn page_count(&self) -> usize {
        self.sizes.len().min(self.pages.len())
    }

    pub fn has_pages(&self) -> bool {
        self.page_count() > 0
    }

    fn dpi_scale(&self) -> f64 {
        self.dpi as f64 / 96.0
    }

    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.dpi_scale()).round() as i32
    }

    /// Mostra il documento `doc` con le pagine `sizes` (DIP), ripristinando `view` se c'è.
    /// `damaged`: il file era danneggiato, in fondo si mostra la pagina "Broken PDF format".
    pub fn set_document(&mut self, doc: Option<u64>, sizes: Vec<(f32, f32)>, view: Option<SavedView>, damaged: bool) {
        self.doc = doc;
        self.sizes = sizes;
        self.damaged = damaged;
        self.message = None;
        self.drag = None;
        self.failed.clear();
        match view {
            Some(v) => {
                self.mode = v.mode;
                self.zoom = v.zoom;
                self.layout();
                if !self.pages.is_empty() {
                    let page = v.page.min(self.pages.len() - 1);
                    self.apply_anchor(Anchor { page, fx: 0.5, fy: v.fy, cx: self.cw / 2, cy: 0 });
                }
            }
            None => {
                self.mode = ZoomMode::FitWidth;
                self.layout();
                self.sx = 0;
                self.sy = 0;
                self.clamp_scroll();
            }
        }
        self.update_scrollbars();
        self.invalidate();
    }

    /// Mostra un messaggio al posto delle pagine.
    pub fn show_message(&mut self, doc: Option<u64>, text: &str) {
        self.doc = doc;
        self.damaged = false;
        self.sizes.clear();
        self.pages.clear();
        self.content_w = 0;
        self.content_h = 0;
        self.sx = 0;
        self.sy = 0;
        self.message = Some(text.to_string());
        self.update_scrollbars();
        self.invalidate();
    }

    pub fn save_view(&self) -> Option<SavedView> {
        if self.pages.is_empty() {
            return None;
        }
        let a = self.anchor_at(self.cw / 2, 0)?;
        Some(SavedView { mode: self.mode, zoom: self.zoom, page: a.page, fy: a.fy })
    }

    /// Dimentica le immagini di un documento chiuso.
    pub fn forget_document(&mut self, doc: u64) {
        self.cache.remove_where(|k, _| k.doc == doc);
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(self.hwnd, None, false);
        }
    }

    fn read_client_size(&mut self) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        self.cw = (rc.right - rc.left).max(1);
        self.ch = (rc.bottom - rc.top).max(1);
    }

    /// Ricalcola posizione e dimensioni delle pagine per zoom e finestra attuali.
    fn layout(&mut self) {
        self.read_client_size();
        if self.sizes.is_empty() {
            self.pages.clear();
            self.content_w = 0;
            self.content_h = 0;
            return;
        }
        let s = self.dpi_scale();
        let margin = self.px(12);
        let gap = self.px(12);
        let max_w = self.sizes.iter().map(|p| p.0).fold(1.0f32, f32::max) as f64;
        let avail_w = (self.cw - 2 * margin).max(16) as f64;
        let avail_h = (self.ch - 2 * margin).max(16) as f64;
        match self.mode {
            ZoomMode::FitWidth => self.zoom = avail_w / (max_w * s),
            ZoomMode::FitPage => {
                let cur = self.current_page().min(self.sizes.len() - 1);
                let (w, h) = self.sizes[cur];
                self.zoom = (avail_w / (w as f64 * s)).min(avail_h / (h as f64 * s));
            }
            ZoomMode::Custom => {}
        }
        self.zoom = self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        // Documenti lunghissimi a zoom forte: l'altezza totale deve restare nei limiti delle
        // barre di scorrimento di Windows (numeri a 32 bit).
        let total_dip: f64 = self.sizes.iter().map(|p| p.1 as f64).sum::<f64>() * 1.1 + 2000.0;
        let max_scale = 1.5e9 / total_dip;
        if self.zoom * s > max_scale {
            self.zoom = (max_scale / s).max(MIN_ZOOM / 100.0);
        }
        self.scale = quantize_scale(self.zoom * s);
        let max_w_px = (max_w * self.scale).round() as i32;
        self.content_w = max_w_px.saturating_add(2 * margin);
        let width = self.content_w.max(self.cw);
        let mut y = margin;
        self.pages.clear();
        let mut sizes = self.sizes.clone();
        if self.damaged {
            // pagina finale "Broken PDF format", grande come l'ultima vera
            sizes.push(*self.sizes.last().unwrap());
        }
        for size in sizes {
            let (w, h) = page_px(size, self.scale);
            self.pages.push(PageRect { x: (width - w) / 2, y, w, h });
            y = y.saturating_add(h).saturating_add(gap);
        }
        self.content_h = y - gap + margin;
    }

    fn max_scroll(&self) -> (i32, i32) {
        ((self.content_w - self.cw).max(0), (self.content_h - self.ch).max(0))
    }

    fn clamp_scroll(&mut self) {
        let (mx, my) = self.max_scroll();
        self.sx = self.sx.clamp(0, mx);
        self.sy = self.sy.clamp(0, my);
    }

    pub fn update_scrollbars(&self) {
        unsafe {
            let v = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS | SIF_DISABLENOSCROLL,
                nMin: 0,
                nMax: (self.content_h - 1).max(0),
                nPage: self.ch.max(0) as u32,
                nPos: self.sy,
                nTrackPos: 0,
            };
            SetScrollInfo(self.hwnd, SB_VERT, &v, true);
            let h = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: if self.content_w > self.cw { self.content_w - 1 } else { 0 },
                nPage: if self.content_w > self.cw { self.cw as u32 } else { 0 },
                nPos: self.sx,
                nTrackPos: 0,
            };
            SetScrollInfo(self.hwnd, SB_HORZ, &h, true);
        }
    }

    /// Punto del documento sotto (cx, cy) della finestra.
    pub fn anchor_at(&self, cx: i32, cy: i32) -> Option<Anchor> {
        if self.pages.is_empty() {
            return None;
        }
        let dy = self.sy + cy;
        let dx = self.sx + cx;
        let i = self.pages.partition_point(|p| p.y <= dy).saturating_sub(1);
        let p = self.pages[i];
        Some(Anchor { page: i, fx: (dx - p.x) as f64 / p.w as f64, fy: (dy - p.y) as f64 / p.h as f64, cx, cy })
    }

    fn apply_anchor(&mut self, a: Anchor) {
        if let Some(p) = self.pages.get(a.page).copied() {
            self.sx = (p.x as f64 + a.fx * p.w as f64).round() as i32 - a.cx;
            self.sy = (p.y as f64 + a.fy * p.h as f64).round() as i32 - a.cy;
        }
        self.clamp_scroll();
    }

    /// La finestra ha cambiato dimensioni (o DPI): ricalcola tenendo ferma la parte in alto.
    pub fn resized(&mut self) {
        let anchor = self.anchor_at(self.cw / 2, 0);
        self.layout();
        if let Some(a) = anchor {
            self.apply_anchor(Anchor { cx: self.cw / 2, ..a });
        }
        self.clamp_scroll();
        self.update_scrollbars();
        self.invalidate();
    }

    /// Imposta modo e livello di zoom tenendo fermo il punto (cx, cy).
    pub fn set_zoom(&mut self, mode: ZoomMode, zoom: f64, at: Option<(i32, i32)>) {
        let (cx, cy) = at.unwrap_or((self.cw / 2, self.ch / 2));
        let anchor = if mode == ZoomMode::Custom { self.anchor_at(cx, cy) } else { self.anchor_at(self.cw / 2, 0).map(|a| Anchor { fy: 0.0, ..a }) };
        // Adattando, la pagina corrente va mostrata dall'inizio.
        let anchor = anchor.map(|a| if mode == ZoomMode::Custom { a } else { self.top_of_page_anchor(a.page) });
        self.mode = mode;
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.layout();
        if let Some(a) = anchor {
            self.apply_anchor(a);
        }
        self.update_scrollbars();
        self.invalidate();
    }

    fn top_of_page_anchor(&self, page: usize) -> Anchor {
        let page = page.min(self.pages.len().saturating_sub(1));
        let margin = self.px(12);
        let fy = match self.pages.get(page) {
            Some(p) if p.h > 0 => -(margin as f64) / p.h as f64,
            _ => 0.0,
        };
        // pagina corrente = quella al centro: per il "fit" si usa quella in cima alla vista
        Anchor { page, fx: 0.5, fy, cx: self.cw / 2, cy: 0 }
    }

    pub fn zoom_step(&mut self, up: bool, at: Option<(i32, i32)>) {
        let z = self.zoom;
        let next = if up { ZOOM_STEPS.iter().copied().find(|&s| s > z * 1.01).unwrap_or(MAX_ZOOM) } else { ZOOM_STEPS.iter().rev().copied().find(|&s| s < z / 1.01).unwrap_or(MIN_ZOOM) };
        self.set_zoom(ZoomMode::Custom, next, at);
    }

    /// Zoom continuo (rotellina con Ctrl, pizzico sul touchpad).
    pub fn zoom_by(&mut self, factor: f64, at: (i32, i32)) {
        let z = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.set_zoom(ZoomMode::Custom, z, Some(at));
    }

    pub fn scroll_to(&mut self, x: i32, y: i32) {
        let (ox, oy) = (self.sx, self.sy);
        self.sx = x;
        self.sy = y;
        self.clamp_scroll();
        if (ox, oy) != (self.sx, self.sy) {
            self.update_scrollbars();
            self.invalidate();
        }
    }

    pub fn scroll_by(&mut self, dx: i32, dy: i32) {
        self.scroll_to(self.sx + dx, self.sy + dy);
    }

    pub fn line_px(&self) -> i32 {
        self.px(48)
    }

    pub fn can_scroll_x(&self) -> bool {
        self.content_w > self.cw
    }

    /// Pagina "corrente" (da 0): quella al centro della finestra; la prima o l'ultima agli estremi.
    pub fn current_page(&self) -> usize {
        // la pagina "Broken PDF format" non conta: lì la corrente è l'ultima vera
        self.current_index().min(self.page_count().saturating_sub(1))
    }

    fn current_index(&self) -> usize {
        if self.pages.is_empty() {
            return 0;
        }
        let (_, my) = self.max_scroll();
        if self.sy <= 0 {
            return 0;
        }
        if self.sy >= my {
            // in fondo: l'ultima pagina se è visibile per intero o quasi
            let last = self.pages.len() - 1;
            if self.pages[last].y < self.sy + self.ch / 2 {
                return last;
            }
        }
        let mid = self.sy + self.ch / 2;
        let i = self.pages.partition_point(|p| p.y <= mid).saturating_sub(1);
        // tra due pagine (nello spazio vuoto): quella successiva
        if self.pages[i].y + self.pages[i].h < mid && i + 1 < self.pages.len() {
            i + 1
        } else {
            i
        }
    }

    pub fn go_to_page(&mut self, page: usize) {
        if self.pages.is_empty() {
            return;
        }
        let page = page.min(self.page_count().max(1) - 1);
        let y = self.pages[page].y - self.px(12);
        self.scroll_to(self.sx, y);
        // anche se lo scorrimento non cambia (ultima pagina già visibile) va ridisegnata
        self.invalidate();
    }

    /// Zoom in percentuale, per le etichette.
    pub fn zoom_percent(&self) -> i32 {
        (self.zoom * 100.0).round() as i32
    }

    /// Un riquadro è pronto: lo si mette da parte (se il documento è ancora quello mostrato).
    pub fn add_tile(&mut self, key: TileKey, bmp: &Bitmap) {
        if Some(key.doc) != self.doc || key.page as usize >= self.sizes.len() {
            return;
        }
        let scale = key.scale as f64 / 4096.0;
        let (pw, ph) = page_px(self.sizes[key.page as usize], scale);
        let (tw, th) = tile_dims(pw, ph);
        let (ox, oy) = (key.tx as i32 * tw, key.ty as i32 * th);
        let Some(hbmp) = make_dib(bmp) else { return };
        let used = if key.scale == scale_key(self.scale) { self.frame } else { 0 };
        self.cache.insert(key, Tile { bmp: hbmp, w: bmp.width as i32, h: bmp.height as i32, ox, oy, pw, ph, used });
        self.cache.trim(self.frame);
        self.invalidate();
    }

    /// Il motore non è riuscito a disegnare un riquadro: resta bianco, senza riprovare.
    pub fn tile_failed(&mut self, key: TileKey) {
        if self.failed.len() > 10_000 {
            self.failed.clear();
        }
        self.failed.insert(key);
    }

    fn have_tile(&self, key: &TileKey) -> bool {
        self.cache.map.contains_key(key) || self.failed.contains(key)
    }

    /// Disegna la finestra e aggiorna l'elenco dei riquadri da chiedere al thread di disegno.
    pub fn paint(&mut self, hdc: HDC, worker: &Worker) {
        let (cw, ch) = (self.cw.max(1), self.ch.max(1));
        unsafe {
            let mem = CreateCompatibleDC(hdc);
            let need_new = !matches!(self.backbuf, Some((_, w, h)) if w == cw && h == ch);
            if need_new {
                if let Some((b, _, _)) = self.backbuf.take() {
                    let _ = DeleteObject(b);
                }
                self.backbuf = Some((CreateCompatibleBitmap(hdc, cw, ch), cw, ch));
            }
            let back = self.backbuf.unwrap().0;
            let old_back = SelectObject(mem, back);
            let all = RECT { left: 0, top: 0, right: cw, bottom: ch };
            FillRect(mem, &all, self.bg);

            let mut wants = Vec::new();
            if self.pages.is_empty() || self.doc.is_none() {
                if let Some(msg) = &self.message {
                    self.draw_message(mem, msg);
                }
            } else {
                self.frame += 1;
                let tile_dc = CreateCompatibleDC(mem);
                let old_tile = SelectObject(tile_dc, HGDIOBJ::default());
                self.paint_pages(mem, tile_dc, &mut wants);
                SelectObject(tile_dc, old_tile);
                let _ = DeleteDC(tile_dc);
                self.prefetch(&mut wants);
            }
            let _ = BitBlt(hdc, 0, 0, cw, ch, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old_back);
            let _ = DeleteDC(mem);
            worker.set_wants(wants);
        }
    }

    fn draw_message(&self, dc: HDC, msg: &str) {
        unsafe {
            let old_font = if self.font.is_invalid() { None } else { Some(SelectObject(dc, self.font)) };
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00EE_EEEE));
            let mut text: Vec<u16> = msg.encode_utf16().collect();
            let margin = self.px(24);
            let mut rc = RECT { left: margin, top: 0, right: (self.cw - margin).max(margin + 10), bottom: self.ch };
            let mut calc = rc;
            DrawTextW(dc, &mut text, &mut calc, DT_CALCRECT | DT_WORDBREAK | DT_CENTER | DT_NOPREFIX);
            let h = calc.bottom - calc.top;
            rc.top = (self.ch - h) / 2;
            rc.bottom = rc.top + h;
            DrawTextW(dc, &mut text, &mut rc, DT_WORDBREAK | DT_CENTER | DT_NOPREFIX);
            if let Some(f) = old_font {
                SelectObject(dc, f);
            }
        }
    }

    fn tile_req(&self, doc: u64, page: usize, tx: i32, ty: i32) -> TileReq {
        let p = self.pages[page];
        let (tw, th) = tile_dims(p.w, p.h);
        let key = TileKey { doc, page: page as u32, scale: scale_key(self.scale), tx: tx as u16, ty: ty as u16 };
        if (tw, th) == (p.w, p.h) {
            return TileReq { key, source: None, width: p.w as u32, height: p.h as u32 };
        }
        let x0 = tx * tw;
        let y0 = ty * th;
        let x1 = (x0 + tw).min(p.w);
        let y1 = (y0 + th).min(p.h);
        let s = self.scale as f32;
        TileReq {
            key,
            source: Some((x0 as f32 / s, y0 as f32 / s, (x1 - x0) as f32 / s, (y1 - y0) as f32 / s)),
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        }
    }

    /// Riquadri della pagina `i` che toccano il rettangolo (x0, y0)-(x1, y1) della pagina.
    fn tiles_in(&self, i: usize, x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
        let p = self.pages[i];
        let (tw, th) = tile_dims(p.w, p.h);
        let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(p.w), y1.min(p.h));
        let mut v = Vec::new();
        if x1 <= x0 || y1 <= y0 {
            return v;
        }
        for ty in y0 / th..=(y1 - 1) / th {
            for tx in x0 / tw..=(x1 - 1) / tw {
                v.push((tx, ty));
            }
        }
        v
    }

    unsafe fn paint_pages(&mut self, mem: HDC, tile_dc: HDC, wants: &mut Vec<TileReq>) {
        let doc = self.doc.unwrap();
        let sk = scale_key(self.scale);
        let shadow = self.px(2).max(1);
        let first = self.pages.partition_point(|p| p.y + p.h < self.sy);
        let center_y = self.ch / 2;
        let mut visible_missing: Vec<(i64, TileReq)> = Vec::new();
        for i in first..self.pages.len() {
            let p = self.pages[i];
            let (rx, ry) = (p.x - self.sx, p.y - self.sy);
            if ry > self.ch {
                break;
            }
            let sh = RECT { left: rx + shadow, top: ry + shadow, right: rx + p.w + shadow, bottom: ry + p.h + shadow };
            FillRect(mem, &sh, self.shadow);
            let pr = RECT { left: rx, top: ry, right: rx + p.w, bottom: ry + p.h };
            FillRect(mem, &pr, self.white);
            if i >= self.sizes.len() {
                self.draw_broken_page(mem, pr);
                continue;
            }
            let tiles = self.tiles_in(i, -rx, -ry, self.cw - rx, self.ch - ry);
            let missing: Vec<(i32, i32)> = tiles
                .iter()
                .copied()
                .filter(|&(tx, ty)| !self.have_tile(&TileKey { doc, page: i as u32, scale: sk, tx: tx as u16, ty: ty as u16 }))
                .collect();
            if !missing.is_empty() {
                // Al posto dei riquadri mancanti: quelli di altre scale, adattati.
                let mut others: Vec<TileKey> = self.cache.map.keys().filter(|k| k.doc == doc && k.page == i as u32 && k.scale != sk).copied().collect();
                others.sort_by_key(|k| k.scale);
                SetStretchBltMode(mem, COLORONCOLOR);
                for k in others {
                    let frame = self.frame;
                    let t = self.cache.map.get_mut(&k).unwrap();
                    t.used = frame;
                    let fx = p.w as f64 / t.pw as f64;
                    let fy = p.h as f64 / t.ph as f64;
                    let dx = rx + (t.ox as f64 * fx).round() as i32;
                    let dy = ry + (t.oy as f64 * fy).round() as i32;
                    let dw = ((t.ox + t.w) as f64 * fx).round() as i32 - (t.ox as f64 * fx).round() as i32;
                    let dh = ((t.oy + t.h) as f64 * fy).round() as i32 - (t.oy as f64 * fy).round() as i32;
                    if dx > self.cw || dy > self.ch || dx + dw < 0 || dy + dh < 0 {
                        continue;
                    }
                    SelectObject(tile_dc, t.bmp);
                    let _ = StretchBlt(mem, dx, dy, dw, dh, tile_dc, 0, 0, t.w, t.h, SRCCOPY);
                }
            }
            let (tw, th) = tile_dims(p.w, p.h);
            let mut page_failed = false;
            for (tx, ty) in tiles {
                let key = TileKey { doc, page: i as u32, scale: sk, tx: tx as u16, ty: ty as u16 };
                if let Some(t) = self.cache.map.get_mut(&key) {
                    t.used = self.frame;
                    SelectObject(tile_dc, t.bmp);
                    let _ = BitBlt(mem, rx + tx * tw, ry + ty * th, t.w, t.h, tile_dc, 0, 0, SRCCOPY);
                } else if self.failed.contains(&key) {
                    page_failed = true;
                } else {
                    let req = self.tile_req(doc, i, tx, ty);
                    let cy = ry + ty * th + th / 2;
                    visible_missing.push((((cy - center_y) as i64).abs(), req));
                }
            }
            if page_failed {
                let visible = RECT { left: pr.left.max(0), top: pr.top.max(0), right: pr.right.min(self.cw), bottom: pr.bottom.min(self.ch) };
                self.draw_text_in(mem, visible, "Pagina danneggiata:\nnon si può mostrare.", COLORREF(0x0000_00B0), 0);
            }
        }
        visible_missing.sort_by_key(|e| e.0);
        wants.extend(visible_missing.into_iter().map(|e| e.1));
    }

    /// Pagina finale dei PDF danneggiati: "Broken PDF format" e una spiegazione.
    unsafe fn draw_broken_page(&self, dc: HDC, page: RECT) {
        let w = page.right - page.left;
        let h = page.bottom - page.top;
        let margin = w / 12;
        let band = RECT { left: page.left, top: page.top + h / 3, right: page.right, bottom: page.top + h / 3 + h / 6 };
        let red = CreateSolidBrush(COLORREF(0x0027_30D6)); // RGB(214, 48, 39)
        FillRect(dc, &band, red);
        let _ = DeleteObject(red);
        let title = RECT { left: band.left + margin, ..band };
        let title = RECT { right: band.right - margin, ..title };
        self.draw_text_in(dc, title, "Broken PDF format", COLORREF(0x00FF_FFFF), (w / 11).max(8));
        let below = RECT { left: page.left + margin, top: band.bottom + h / 24, right: page.right - margin, bottom: page.bottom - h / 8 };
        self.draw_text_in(
            dc,
            below,
            "Il file è danneggiato o incompleto.\nLe pagine precedenti sono quelle che è stato possibile leggere; il resto del documento è perso.",
            COLORREF(0x0040_4040),
            (w / 34).max(7),
        );
    }

    /// Testo centrato in `rc`, con un font alto `height` pixel (0 = quello dell'interfaccia).
    unsafe fn draw_text_in(&self, dc: HDC, rc: RECT, text: &str, color: COLORREF, height: i32) {
        if rc.right - rc.left < 8 || rc.bottom - rc.top < 8 {
            return;
        }
        let font = if height > 0 {
            let face: Vec<u16> = "Segoe UI".encode_utf16().chain(std::iter::once(0)).collect();
            let mut lf = LOGFONTW { lfHeight: -height, lfWeight: 600, lfQuality: CLEARTYPE_QUALITY, ..Default::default() };
            lf.lfFaceName[..face.len()].copy_from_slice(&face);
            CreateFontIndirectW(&lf)
        } else {
            HFONT::default()
        };
        let use_font = if height > 0 { font } else { self.font };
        let old = if use_font.is_invalid() { None } else { Some(SelectObject(dc, use_font)) };
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, color);
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        let mut calc = rc;
        DrawTextW(dc, &mut buf, &mut calc, DT_CALCRECT | DT_WORDBREAK | DT_CENTER | DT_NOPREFIX);
        let th = calc.bottom - calc.top;
        let mut out = RECT { top: rc.top + ((rc.bottom - rc.top) - th).max(0) / 2, ..rc };
        out.bottom = out.top + th;
        DrawTextW(dc, &mut buf, &mut out, DT_WORDBREAK | DT_CENTER | DT_NOPREFIX);
        if let Some(o) = old {
            SelectObject(dc, o);
        }
        if height > 0 {
            let _ = DeleteObject(font);
        }
    }

    /// Dopo i riquadri visibili: quelli appena sopra e sotto, per scorrere senza attese.
    fn prefetch(&self, wants: &mut Vec<TileReq>) {
        let Some(doc) = self.doc else { return };
        let sk = scale_key(self.scale);
        let (top, bottom) = (self.sy - self.ch, self.sy + 2 * self.ch);
        let first = self.pages.partition_point(|p| p.y + p.h < top);
        let mut below = Vec::new();
        let mut above = Vec::new();
        for i in first..self.page_count() {
            let p = self.pages[i];
            if p.y > bottom {
                break;
            }
            let (rx, ry) = (p.x - self.sx, p.y - self.sy);
            for (tx, ty) in self.tiles_in(i, -rx, top - p.y, self.cw - rx, bottom - p.y) {
                let key = TileKey { doc, page: i as u32, scale: sk, tx: tx as u16, ty: ty as u16 };
                if self.have_tile(&key) || wants.iter().any(|w| w.key == key) {
                    continue;
                }
                let req = self.tile_req(doc, i, tx, ty);
                if ry < 0 {
                    above.push(req);
                } else {
                    below.push(req);
                }
            }
        }
        // prima quello che viene dopo (si legge verso il basso), poi quello prima
        wants.extend(below);
        above.reverse();
        wants.extend(above);
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        unsafe {
            if let Some((b, _, _)) = self.backbuf.take() {
                let _ = DeleteObject(b);
            }
            let _ = DeleteObject(self.bg);
            let _ = DeleteObject(self.shadow);
            let _ = DeleteObject(self.white);
        }
    }
}

/// Copia una [`Bitmap`] in una DIB di GDI.
fn make_dib(bmp: &Bitmap) -> Option<HBITMAP> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bmp.width as i32,
            biHeight: -(bmp.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    unsafe {
        let h = CreateDIBSection(HDC::default(), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(h);
            return None;
        }
        std::ptr::copy_nonoverlapping(bmp.pixels.as_ptr(), bits as *mut u8, bmp.pixels.len());
        Some(h)
    }
}

