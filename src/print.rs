//! Stampa: le pagine vengono disegnate dal motore PDF di Windows e mandate alla stampante
//! con GDI. Le pagine più grandi del foglio vengono ridotte (mai ingrandite), centrate
//! nell'area stampabile e ruotate se l'orientamento è diverso da quello del foglio.

use crate::render::{init_thread, open_document, Bitmap};
use crate::wide::WStr;
use std::path::Path;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{GlobalFree, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{
    CreateDCW, DeleteDC, GetDeviceCaps, SetBrushOrgEx, SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HALFTONE, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, SRCCOPY, VERTRES,
};
use windows::Win32::Graphics::Printing::{EnumPrintersW, GetDefaultPrinterW, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL, PRINTER_INFO_4W};
use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::UI::Controls::Dialogs::{
    PrintDlgExW, DEVNAMES, PD_ALLPAGES, PD_CURRENTPAGE, PD_NOCURRENTPAGE, PD_NOPAGENUMS, PD_NOSELECTION, PD_PAGENUMS, PD_RESULT_PRINT, PD_RETURNDC,
    PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGEXW, PRINTPAGERANGE, START_PAGE_GENERAL,
};

/// Risoluzione massima usata per disegnare le pagine da stampare (punti per pollice).
const MAX_PRINT_DPI: f32 = 300.0;

/// Contesto di stampa (HDC) che si può passare a un altro thread.
pub struct PrinterDc(pub HDC);
unsafe impl Send for PrinterDc {}

impl Drop for PrinterDc {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
}

/// Nome della stampante predefinita.
pub fn default_printer() -> Option<String> {
    let mut len = 0u32;
    unsafe {
        let _ = GetDefaultPrinterW(PWSTR::null(), &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        if GetDefaultPrinterW(PWSTR(buf.as_mut_ptr()), &mut len).as_bool() {
            Some(crate::wide::from_wide(&buf))
        } else {
            None
        }
    }
}

/// Nomi delle stampanti installate (locali e di rete), in ordine alfabetico.
pub fn list_printers() -> Vec<String> {
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let (mut needed, mut count) = (0u32, 0u32);
    unsafe {
        let _ = EnumPrintersW(flags, PCWSTR::null(), 4, None, &mut needed, &mut count);
        if needed == 0 {
            return Vec::new();
        }
        // buffer allineato come le strutture che conterrà
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let bytes = std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 8);
        if EnumPrintersW(flags, PCWSTR::null(), 4, Some(bytes), &mut needed, &mut count).is_err() {
            return Vec::new();
        }
        let infos = std::slice::from_raw_parts(buf.as_ptr() as *const PRINTER_INFO_4W, count as usize);
        let mut names: Vec<String> = infos.iter().filter(|i| !i.pPrinterName.is_null()).map(|i| i.pPrinterName.to_string().unwrap_or_default()).collect();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }
}

/// Apre la stampante `name` (o la predefinita).
pub fn open_printer(name: Option<&str>) -> Result<PrinterDc, String> {
    let name = match name {
        Some(n) => n.to_string(),
        None => default_printer().ok_or("nessuna stampante predefinita")?,
    };
    let w = WStr::new(&name);
    let hdc = unsafe { CreateDCW(PCWSTR::null(), w.pcwstr(), PCWSTR::null(), None) };
    if hdc.is_invalid() {
        return Err(format!("impossibile aprire la stampante \"{name}\" (l'elenco delle stampanti si ottiene con --printers)"));
    }
    Ok(PrinterDc(hdc))
}

/// Pagine scelte nella finestra di stampa.
pub struct PrintChoice {
    pub dc: PrinterDc,
    /// Pagine da stampare, da 0; `None` = tutte. Le copie le fa il driver della stampante.
    pub pages: Option<Vec<usize>>,
}

/// Impostazioni della stampante ricordate tra una stampa e l'altra.
#[derive(Default)]
pub struct PrinterSettings {
    dev_mode: HGLOBAL,
    dev_names: HGLOBAL,
}

impl Drop for PrinterSettings {
    fn drop(&mut self) {
        unsafe {
            if !self.dev_mode.is_invalid() {
                let _ = GlobalFree(self.dev_mode);
            }
            if !self.dev_names.is_invalid() {
                let _ = GlobalFree(self.dev_names);
            }
        }
    }
}

impl PrinterSettings {
    /// Nome della stampante scelta nell'ultima finestra di stampa.
    pub fn printer_name(&self) -> Option<String> {
        if self.dev_names.is_invalid() {
            return None;
        }
        unsafe {
            let base = GlobalLock(self.dev_names) as *const u16;
            if base.is_null() {
                return None;
            }
            let dn = &*(base as *const DEVNAMES);
            let start = base.add(dn.wDeviceOffset as usize);
            let mut len = 0;
            while *start.add(len) != 0 && len < 1024 {
                len += 1;
            }
            let name = String::from_utf16_lossy(std::slice::from_raw_parts(start, len));
            let _ = GlobalUnlock(self.dev_names);
            Some(name)
        }
    }
}

/// Come presentare la finestra di stampa.
pub struct DialogOptions {
    /// Pagine del documento (per scegliere gli intervalli); `None` = solo "Tutte".
    pub page_count: Option<usize>,
    /// Pagina mostrata (da 0), per la scelta "Pagina corrente"; `None` = scelta assente.
    pub current_page: Option<usize>,
    /// Pagine (da 0) già scelte nella finestra.
    pub preset_pages: Option<Vec<usize>>,
    pub copies: u32,
}

/// Indici di pagina (da 0) raggruppati in intervalli da 1 per la finestra di stampa.
fn to_ranges(pages: &[usize]) -> Vec<PRINTPAGERANGE> {
    let mut out: Vec<PRINTPAGERANGE> = Vec::new();
    for &p in pages {
        let n = p as u32 + 1;
        match out.last_mut() {
            Some(r) if r.nToPage + 1 == n => r.nToPage = n,
            _ => out.push(PRINTPAGERANGE { nFromPage: n, nToPage: n }),
        }
    }
    out
}

/// Mostra la finestra "Stampa" di Windows. `None` se l'utente annulla.
pub fn print_dialog(owner: HWND, settings: &mut PrinterSettings, opts: &DialogOptions) -> Result<Option<PrintChoice>, String> {
    let page_count = opts.page_count.unwrap_or(0);
    let mut ranges = vec![PRINTPAGERANGE { nFromPage: 1, nToPage: page_count.max(1) as u32 }; 32];
    let mut flags = PD_RETURNDC | PD_USEDEVMODECOPIESANDCOLLATE | PD_NOSELECTION;
    let mut n_ranges = 0;
    if opts.page_count.is_none() {
        flags |= PD_NOPAGENUMS;
    }
    if opts.current_page.is_none() {
        flags |= PD_NOCURRENTPAGE;
    }
    match opts.preset_pages.as_deref().map(to_ranges) {
        Some(preset) if !preset.is_empty() && preset.len() <= ranges.len() && opts.page_count.is_some() => {
            ranges[..preset.len()].copy_from_slice(&preset);
            n_ranges = preset.len() as u32;
            flags |= PD_PAGENUMS;
        }
        _ => flags |= PD_ALLPAGES,
    }
    let mut pd = PRINTDLGEXW {
        lStructSize: std::mem::size_of::<PRINTDLGEXW>() as u32,
        hwndOwner: owner,
        hDevMode: settings.dev_mode,
        hDevNames: settings.dev_names,
        Flags: flags,
        nPageRanges: n_ranges,
        nMaxPageRanges: ranges.len() as u32,
        lpPageRanges: ranges.as_mut_ptr(),
        nMinPage: 1,
        nMaxPage: page_count.max(1) as u32,
        nCopies: opts.copies.max(1),
        nStartPage: START_PAGE_GENERAL,
        ..Default::default()
    };
    unsafe { PrintDlgExW(&mut pd) }.map_err(|e| format!("finestra di stampa: {}", e.message()))?;
    // Windows può aver riallocato le impostazioni: si tengono quelle nuove.
    settings.dev_mode = pd.hDevMode;
    settings.dev_names = pd.hDevNames;
    let dc = PrinterDc(pd.hDC);
    if pd.dwResultAction != PD_RESULT_PRINT {
        return Ok(None);
    }
    if dc.0.is_invalid() {
        return Err("la stampante scelta non è disponibile".into());
    }
    let pages: Option<Vec<usize>> = if pd.Flags.0 & PD_PAGENUMS.0 != 0 {
        let mut v = Vec::new();
        for r in &ranges[..pd.nPageRanges as usize] {
            let (a, b) = (r.nFromPage.min(r.nToPage), r.nFromPage.max(r.nToPage));
            for p in a..=b {
                if p >= 1 && (p as usize) <= page_count {
                    v.push(p as usize - 1);
                }
            }
        }
        Some(v)
    } else if pd.Flags.0 & PD_CURRENTPAGE.0 != 0 {
        opts.current_page.map(|c| vec![c.min(page_count.saturating_sub(1))])
    } else {
        None
    };
    Ok(Some(PrintChoice { dc, pages }))
}

/// Interpreta un elenco di pagine come "1-3,5,8-" (da 1) e restituisce gli indici da 0.
pub fn parse_page_ranges(spec: &str, page_count: usize) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in spec.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()) {
        let bad = || format!("intervallo di pagine non valido: \"{part}\"");
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => {
                let a = if a.trim().is_empty() { 1 } else { a.trim().parse::<usize>().map_err(|_| bad())? };
                let b = if b.trim().is_empty() { page_count } else { b.trim().parse::<usize>().map_err(|_| bad())? };
                (a, b)
            }
            None => {
                let n = part.parse::<usize>().map_err(|_| bad())?;
                (n, n)
            }
        };
        if a == 0 || b == 0 || a > page_count || b > page_count {
            return Err(format!("pagina fuori dal documento ({page_count} pagine): \"{part}\""));
        }
        if a <= b {
            out.extend(a - 1..b);
        } else {
            out.extend((b - 1..a).rev());
        }
    }
    if out.is_empty() {
        return Err("nessuna pagina indicata".into());
    }
    Ok(out)
}

/// Come sistemare una pagina sul foglio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Ruotare la pagina di 90°.
    pub rotate: bool,
    /// Rettangolo sul foglio, in pixel della stampante, relativo all'area stampabile.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Dimensioni della bitmap da disegnare (già ruotata).
    pub render_width: u32,
    pub render_height: u32,
}

/// Calcola la posizione di una pagina di `page_dip` (DIP) su un'area stampabile di
/// `area_px` pixel a `dpi` punti per pollice.
pub fn place_page(page_dip: (f32, f32), area_px: (i32, i32), dpi: (i32, i32)) -> Placement {
    let (dpix, dpiy) = (dpi.0.max(1) as f32, dpi.1.max(1) as f32);
    let (aw, ah) = (area_px.0.max(1) as f32 / dpix, area_px.1.max(1) as f32 / dpiy);
    let (mut pw, mut ph) = (page_dip.0 / 96.0, page_dip.1 / 96.0);
    let page_landscape = pw > ph * 1.01;
    let page_portrait = ph > pw * 1.01;
    let area_landscape = aw > ah;
    let rotate = (page_landscape && !area_landscape) || (page_portrait && area_landscape);
    if rotate {
        std::mem::swap(&mut pw, &mut ph);
    }
    let scale = (aw / pw).min(ah / ph).min(1.0);
    let width = (pw * scale * dpix).round().max(1.0) as i32;
    let height = (ph * scale * dpiy).round().max(1.0) as i32;
    let x = (area_px.0 - width) / 2;
    let y = (area_px.1 - height) / 2;
    let (rdx, rdy) = (dpix.min(MAX_PRINT_DPI), dpiy.min(MAX_PRINT_DPI));
    let render_width = (pw * scale * rdx).round().max(1.0) as u32;
    let render_height = (ph * scale * rdy).round().max(1.0) as u32;
    Placement { rotate, x: x.max(0), y: y.max(0), width, height, render_width, render_height }
}

/// Stampa le pagine `pages` (da 0; vuoto = tutte) del PDF `bytes` sul contesto `dc`.
///
/// `output_file` manda la stampa in un file (per le stampanti che lo permettono, come
/// "Microsoft Print to PDF"). `progress(fatte, totali)` viene chiamata dopo ogni pagina.
/// Restituisce il numero di pagine stampate. Di un PDF danneggiato si stampano le pagine
/// leggibili.
pub fn print_document(
    dc: &PrinterDc,
    bytes: &[u8],
    password: Option<&str>,
    title: &str,
    pages: &[usize],
    output_file: Option<&Path>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<usize, String> {
    init_thread();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| print_document_inner(dc, bytes, password, title, pages, output_file, progress)))
        .unwrap_or_else(|_| Err("errore interno durante la stampa".into()))
}

fn print_document_inner(
    dc: &PrinterDc,
    bytes: &[u8],
    password: Option<&str>,
    title: &str,
    pages: &[usize],
    output_file: Option<&Path>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<usize, String> {
    let doc = open_document(bytes, password).map_err(|e| e.to_string())?.doc;
    let pages: Vec<usize> = if pages.is_empty() { (0..doc.page_count()).collect() } else { pages.iter().copied().filter(|&p| p < doc.page_count()).collect() };
    if pages.is_empty() {
        return Err("nessuna pagina da stampare".into());
    }
    let hdc = dc.0;
    let name = WStr::new(title);
    let out = output_file.map(|p| WStr::from_os(p.as_os_str()));
    let info = DOCINFOW {
        cbSize: std::mem::size_of::<DOCINFOW>() as i32,
        lpszDocName: name.pcwstr(),
        lpszOutput: out.as_ref().map(|o| o.pcwstr()).unwrap_or(PCWSTR::null()),
        ..Default::default()
    };
    if unsafe { StartDocW(hdc, &info) } <= 0 {
        return Err("la stampante ha rifiutato il lavoro".into());
    }
    let result = (|| -> Result<(), String> {
        let area = unsafe { (GetDeviceCaps(hdc, HORZRES), GetDeviceCaps(hdc, VERTRES)) };
        let dpi = unsafe { (GetDeviceCaps(hdc, LOGPIXELSX), GetDeviceCaps(hdc, LOGPIXELSY)) };
        for (n, &p) in pages.iter().enumerate() {
            let place = place_page(doc.page_size(p), area, dpi);
            let bmp = if place.rotate {
                doc.render(p, None, place.render_height, place.render_width)?.rotated_cw()
            } else {
                doc.render(p, None, place.render_width, place.render_height)?
            };
            if unsafe { StartPage(hdc) } <= 0 {
                return Err("errore della stampante all'inizio della pagina".into());
            }
            blit_to_printer(hdc, &bmp, &place)?;
            if unsafe { EndPage(hdc) } <= 0 {
                return Err("errore della stampante alla fine della pagina".into());
            }
            progress(n + 1, pages.len());
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            if unsafe { EndDoc(hdc) } <= 0 {
                return Err("errore della stampante alla chiusura del lavoro".into());
            }
            Ok(pages.len())
        }
        Err(e) => {
            unsafe {
                AbortDoc(hdc);
            }
            Err(e)
        }
    }
}

/// Copia la bitmap sulla stampante, a strisce (alcuni driver non gradiscono immagini enormi)
/// e a 24 bit (i 32 bit non sono supportati da tutti i driver).
fn blit_to_printer(hdc: HDC, bmp: &Bitmap, place: &Placement) -> Result<(), String> {
    const BAND: u32 = 512;
    let (w, h) = (bmp.width, bmp.height);
    let stride = ((w * 3 + 3) & !3) as usize;
    unsafe {
        SetStretchBltMode(hdc, HALFTONE);
        let _ = SetBrushOrgEx(hdc, 0, 0, None);
    }
    let mut row0 = 0u32;
    let mut band = vec![0u8; stride * BAND.min(h) as usize];
    while row0 < h {
        let rows = BAND.min(h - row0);
        for r in 0..rows {
            let src = &bmp.pixels[((row0 + r) * w * 4) as usize..((row0 + r + 1) * w * 4) as usize];
            let dst = &mut band[r as usize * stride..r as usize * stride + (w * 3) as usize];
            for (d, s) in dst.as_chunks_mut::<3>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                d.copy_from_slice(&s[..3]);
            }
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(rows as i32),
                biPlanes: 1,
                biBitCount: 24,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let y0 = place.y + (row0 as i64 * place.height as i64 / h as i64) as i32;
        let y1 = place.y + ((row0 + rows) as i64 * place.height as i64 / h as i64) as i32;
        let done = unsafe {
            StretchDIBits(hdc, place.x, y0, place.width, (y1 - y0).max(1), 0, 0, w as i32, rows as i32, Some(band.as_ptr() as *const _), &bmi, DIB_RGB_COLORS, SRCCOPY)
        };
        if done == 0 {
            return Err("la stampante non ha accettato l'immagine della pagina".into());
        }
        row0 += rows;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervalli_di_pagine() {
        assert_eq!(parse_page_ranges("1-3,5", 10).unwrap(), vec![0, 1, 2, 4]);
        assert_eq!(parse_page_ranges("8-", 10).unwrap(), vec![7, 8, 9]);
        assert_eq!(parse_page_ranges("-2", 10).unwrap(), vec![0, 1]);
        assert_eq!(parse_page_ranges("3-1", 10).unwrap(), vec![2, 1, 0]);
        assert!(parse_page_ranges("0", 10).is_err());
        assert!(parse_page_ranges("11", 10).is_err());
        assert!(parse_page_ranges("a-b", 10).is_err());
        assert!(parse_page_ranges("", 10).is_err());
    }

    #[test]
    fn intervalli_per_la_finestra() {
        let r: Vec<(u32, u32)> = to_ranges(&[0, 1, 2, 4, 7, 8]).iter().map(|r| (r.nFromPage, r.nToPage)).collect();
        assert_eq!(r, vec![(1, 3), (5, 5), (8, 9)]);
        assert!(to_ranges(&[]).is_empty());
    }

    #[test]
    fn posizione_sul_foglio() {
        // A4 verticale (DIP) su area A4 a 600 dpi leggermente più piccola del foglio: ridotta, centrata.
        let a4 = (793.7, 1122.5);
        let area = (4760, 6814);
        let p = place_page(a4, area, (600, 600));
        assert!(!p.rotate);
        assert!(p.width <= area.0 && p.height <= area.1);
        assert!(p.width >= area.0 - 2 || p.height >= area.1 - 2);
        assert_eq!(p.x, (area.0 - p.width) / 2);
        // A 600 dpi si disegna a 300: metà dei pixel.
        assert!((p.render_width as i32 - p.width / 2).abs() <= 1);
        // A4 orizzontale su foglio verticale: ruotata.
        let p = place_page((a4.1, a4.0), area, (600, 600));
        assert!(p.rotate);
        assert!(p.height > p.width);
        // A5 su A4: dimensioni reali, non ingrandita.
        let p = place_page((559.4, 793.7), area, (600, 600));
        assert!(!p.rotate);
        assert!((p.width - (559.4f32 / 96.0 * 600.0).round() as i32).abs() <= 1);
        // Pagina quadrata: mai ruotata.
        assert!(!place_page((500.0, 500.0), (6000, 4000), (600, 600)).rotate);
    }
}
