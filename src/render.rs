//! Disegno delle pagine PDF con il motore integrato in Windows 10/11 (`Windows.Data.Pdf`).
//!
//! Ogni thread che usa queste funzioni deve prima chiamare [`init_thread`]. Le pagine escono
//! come bitmap BGRA (dall'alto in basso), pronte per GDI.

use windows::core::{Error as WinError, HRESULT, HSTRING};
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Foundation::Rect;
use windows::Graphics::Imaging::BitmapEncoder;
use windows::Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream};
use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_UNAWARE};
use windows::UI::Color;

/// Lato massimo, in pixel, di una bitmap chiesta al motore (oltre si disegna a riquadri).
pub const MAX_RENDER_SIDE: u32 = 16384;

/// Immagine BGRA a 32 bit, righe dall'alto in basso, `width * 4` byte per riga.
#[derive(Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Bitmap {
    /// Pixel (B, G, R, A) in (x, y).
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2], self.pixels[i + 3]]
    }

    /// La stessa immagine ruotata di 90° in senso orario.
    pub fn rotated_cw(&self) -> Bitmap {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut out = vec![0u8; w * h * 4];
        // (x, y) → (h - 1 - y, x) nella nuova immagine larga h
        for y in 0..h {
            let src_row = &self.pixels[y * w * 4..(y + 1) * w * 4];
            let nx = h - 1 - y;
            for x in 0..w {
                let d = (x * h + nx) * 4;
                out[d..d + 4].copy_from_slice(&src_row[x * 4..x * 4 + 4]);
            }
        }
        Bitmap { width: self.height, height: self.width, pixels: out }
    }
}

/// Perché un PDF non si apre.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadError {
    /// Serve una password (o quella data è sbagliata).
    PasswordRequired,
    /// Il file non è un PDF (altro formato).
    NotPdf,
    /// È un PDF, ma così danneggiato che nessuna pagina è leggibile.
    Broken(String),
    Other(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::PasswordRequired => write!(f, "il PDF è protetto da password"),
            LoadError::NotPdf => write!(f, "non è un file PDF (formato non supportato)"),
            LoadError::Broken(_) => write!(f, "PDF danneggiato: nessuna pagina leggibile (broken PDF format)"),
            LoadError::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Un PDF aperto da [`open_document`].
pub struct Opened {
    pub doc: PdfDoc,
    /// Il file è danneggiato o troncato: `doc` contiene solo le pagine recuperate.
    pub damaged: bool,
}

/// Apre un file qualsiasi in modo robusto:
/// - se non è un PDF → [`LoadError::NotPdf`];
/// - se è un PDF danneggiato o troncato → le pagine leggibili, con `damaged` vero;
/// - se non si recupera nessuna pagina → [`LoadError::Broken`].
///
/// Un errore inatteso durante il recupero non chiude mai il programma.
pub fn open_document(bytes: &[u8], password: Option<&str>) -> Result<Opened, LoadError> {
    if !crate::repair::looks_like_pdf(bytes) {
        return Err(LoadError::NotPdf);
    }
    let salvage = || std::panic::catch_unwind(|| crate::repair::salvage(bytes)).ok().flatten();
    // Senza %%EOF in fondo il file è quasi certamente troncato.
    let truncated = !crate::repair::has_eof_marker(bytes);
    match PdfDoc::load(bytes, password) {
        Ok(doc) if !truncated => Ok(Opened { doc, damaged: false }),
        Ok(doc) => {
            // Il motore ha aperto il file, ma le ultime pagine potrebbero mancare: meglio una
            // copia ripulita con le sole pagine leggibili, se non ne perde.
            if let Some(s) = salvage().filter(|s| s.pages >= doc.page_count()) {
                if let Ok(clean) = PdfDoc::load(&s.pdf, None) {
                    return Ok(Opened { doc: clean, damaged: true });
                }
            }
            Ok(Opened { doc, damaged: true })
        }
        Err(LoadError::PasswordRequired) => Err(LoadError::PasswordRequired),
        Err(e) => {
            let detail = e.to_string();
            match salvage() {
                Some(s) => PdfDoc::load(&s.pdf, None).map(|doc| Opened { doc, damaged: true }).map_err(|_| LoadError::Broken(detail)),
                None => Err(LoadError::Broken(detail)),
            }
        }
    }
}

/// ERROR_WRONG_PASSWORD: lo restituisce Windows.Data.Pdf per i PDF con password di apertura.
const E_WRONG_PASSWORD: HRESULT = HRESULT(0x8007_052B_u32 as i32);

/// Prepara il thread corrente per le API WinRT (apartment multithread). Si può chiamare più volte.
///
/// Da chiamare solo su thread che non creano finestre: il thread diventa "DPI unaware",
/// perché in un processo DPI aware il motore PDF moltiplicherebbe le dimensioni chieste
/// per la scala dello schermo (125%, 150%…).
pub fn init_thread() {
    // S_FALSE (già inizializzato) e RPC_E_CHANGED_MODE (thread già STA) vanno bene comunque:
    // gli oggetti di Windows.Data.Pdf sono agili.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE);
    }
}

/// Fattore con cui il motore ingrandisce le dimensioni chieste (1 se le rispetta), imparato
/// dai disegni già fatti. Serve se, nonostante [`init_thread`], il motore scala lo stesso.
static ENGINE_SCALE: AtomicU32 = AtomicU32::new(0x3F80_0000); // 1.0f32

fn engine_scale() -> f32 {
    f32::from_bits(ENGINE_SCALE.load(Ordering::Relaxed))
}

/// Porta `bmp` a esattamente `w` × `h`: ritaglia o aggiunge bianco per differenze di
/// arrotondamento, altrimenti ridimensiona.
fn fit_exact(bmp: Bitmap, w: u32, h: u32) -> Bitmap {
    if bmp.width == w && bmp.height == h {
        return bmp;
    }
    let small = bmp.width.abs_diff(w) <= 3 && bmp.height.abs_diff(h) <= 3;
    let mut out = vec![255u8; (w * h * 4) as usize];
    for y in 0..h {
        let sy = if small { y } else { (y as u64 * bmp.height as u64 / h as u64) as u32 };
        if sy >= bmp.height {
            continue;
        }
        let dst = &mut out[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
        if small {
            let n = w.min(bmp.width) as usize * 4;
            let src = (sy * bmp.width * 4) as usize;
            dst[..n].copy_from_slice(&bmp.pixels[src..src + n]);
        } else {
            for x in 0..w {
                let sx = (x as u64 * bmp.width as u64 / w as u64) as u32;
                let s = ((sy * bmp.width + sx) * 4) as usize;
                dst[(x * 4) as usize..(x * 4 + 4) as usize].copy_from_slice(&bmp.pixels[s..s + 4]);
            }
        }
    }
    Bitmap { width: w, height: h, pixels: out }
}

fn win_err(context: &str, e: WinError) -> String {
    let msg = e.message().to_string();
    if msg.trim().is_empty() {
        format!("{context}: errore 0x{:08X}", e.code().0 as u32)
    } else {
        format!("{context}: {} (0x{:08X})", msg.trim(), e.code().0 as u32)
    }
}

/// Un PDF aperto con il motore di Windows.
pub struct PdfDoc {
    doc: PdfDocument,
    // Il motore legge le pagine dallo stream quando servono: resta vivo quanto il documento.
    _stream: InMemoryRandomAccessStream,
    sizes: Vec<(f32, f32)>,
}

impl PdfDoc {
    /// Apre un PDF dai suoi byte. `password` serve solo per i PDF con password di apertura.
    pub fn load(bytes: &[u8], password: Option<&str>) -> Result<PdfDoc, LoadError> {
        let other = |c: &str, e: WinError| LoadError::Other(win_err(c, e));
        let stream = InMemoryRandomAccessStream::new().map_err(|e| other("stream", e))?;
        {
            let writer = DataWriter::CreateDataWriter(&stream).map_err(|e| other("stream", e))?;
            writer.WriteBytes(bytes).map_err(|e| other("stream", e))?;
            writer.StoreAsync().and_then(|op| op.get()).map_err(|e| other("stream", e))?;
            writer.FlushAsync().and_then(|op| op.get()).map_err(|e| other("stream", e))?;
            // Senza DetachStream, chiudere il writer chiuderebbe anche lo stream.
            writer.DetachStream().map_err(|e| other("stream", e))?;
        }
        stream.Seek(0).map_err(|e| other("stream", e))?;
        let op = match password {
            Some(p) => PdfDocument::LoadFromStreamWithPasswordAsync(&stream, &HSTRING::from(p)),
            None => PdfDocument::LoadFromStreamAsync(&stream),
        };
        let doc = match op.and_then(|op| op.get()) {
            Ok(d) => d,
            Err(e) if e.code() == E_WRONG_PASSWORD => return Err(LoadError::PasswordRequired),
            Err(e) => return Err(other("PDF non valido o non supportato", e)),
        };
        let count = doc.PageCount().map_err(|e| other("pagine", e))?;
        let mut sizes = Vec::with_capacity(count as usize);
        for i in 0..count {
            let page = doc.GetPage(i).map_err(|e| other("pagina", e))?;
            let s = page.Size().map_err(|e| other("pagina", e))?;
            let _ = page.Close();
            // pagine senza dimensioni sensate: A4; e mai oltre il massimo del formato PDF
            // (200 pollici), così un valore assurdo non manda fuori scala la vista
            let ok = |v: f32| v.is_finite() && v > 0.5;
            let (w, h) = if ok(s.Width) && ok(s.Height) { (s.Width, s.Height) } else { (793.7, 1122.5) };
            sizes.push((w.clamp(2.0, 19200.0), h.clamp(2.0, 19200.0)));
        }
        Ok(PdfDoc { doc, _stream: stream, sizes })
    }

    pub fn page_count(&self) -> usize {
        self.sizes.len()
    }

    /// Dimensioni della pagina in DIP (1/96 di pollice), già ruotata come va mostrata.
    pub fn page_size(&self, page: usize) -> (f32, f32) {
        self.sizes[page]
    }

    pub fn page_sizes(&self) -> &[(f32, f32)] {
        &self.sizes
    }

    /// Disegna la pagina `page` (da 0) in una bitmap `width` × `height`.
    ///
    /// `source` è il rettangolo della pagina da disegnare, in DIP (x, y, larghezza, altezza);
    /// `None` = la pagina intera.
    pub fn render(&self, page: usize, source: Option<(f32, f32, f32, f32)>, width: u32, height: u32) -> Result<Bitmap, String> {
        if page >= self.sizes.len() {
            return Err(format!("pagina {} inesistente", page + 1));
        }
        let width = width.clamp(1, MAX_RENDER_SIDE);
        let height = height.clamp(1, MAX_RENDER_SIDE);
        let f = engine_scale();
        let ask = |v: u32| ((v as f32 / f).round() as u32).max(1);
        let mut bmp = self.render_raw(page, source, ask(width), ask(height))?;
        if bmp.width != width || bmp.height != height {
            // Il motore ha scalato: si impara il fattore e si riprova una volta.
            let learned = bmp.width as f32 / ask(width) as f32;
            if (learned - f).abs() > 0.01 && (0.25..=8.0).contains(&learned) {
                ENGINE_SCALE.store(learned.to_bits(), Ordering::Relaxed);
                let ask = |v: u32| ((v as f32 / learned).round() as u32).max(1);
                bmp = self.render_raw(page, source, ask(width), ask(height))?;
            }
        }
        Ok(fit_exact(bmp, width, height))
    }

    fn render_raw(&self, page: usize, source: Option<(f32, f32, f32, f32)>, width: u32, height: u32) -> Result<Bitmap, String> {
        let e = |e: WinError| win_err("disegno della pagina", e);
        let opts = PdfPageRenderOptions::new().map_err(e)?;
        opts.SetDestinationWidth(width).map_err(e)?;
        opts.SetDestinationHeight(height).map_err(e)?;
        if let Some((x, y, w, h)) = source {
            opts.SetSourceRect(Rect { X: x, Y: y, Width: w, Height: h }).map_err(e)?;
        }
        opts.SetBackgroundColor(Color { A: 255, R: 255, G: 255, B: 255 }).map_err(e)?;
        opts.SetIsIgnoringHighContrast(true).map_err(e)?;
        opts.SetBitmapEncoderId(BitmapEncoder::BmpEncoderId().map_err(e)?).map_err(e)?;

        let pdf_page = self.doc.GetPage(page as u32).map_err(e)?;
        let out = InMemoryRandomAccessStream::new().map_err(e)?;
        let result = pdf_page.RenderWithOptionsToStreamAsync(&out, &opts).and_then(|op| op.get());
        let _ = pdf_page.Close();
        result.map_err(e)?;

        let size = out.Size().map_err(e)?;
        if size == 0 || size > u32::MAX as u64 {
            return Err("il motore PDF non ha prodotto l'immagine".into());
        }
        let input = out.GetInputStreamAt(0).map_err(e)?;
        let reader = DataReader::CreateDataReader(&input).map_err(e)?;
        let loaded = reader.LoadAsync(size as u32).and_then(|op| op.get()).map_err(e)?;
        let mut buf = vec![0u8; loaded as usize];
        reader.ReadBytes(&mut buf).map_err(e)?;
        parse_bmp(&buf)
    }
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Legge un file BMP (24 o 32 bit, non compresso o con BI_BITFIELDS) in una [`Bitmap`] BGRA.
pub fn parse_bmp(b: &[u8]) -> Result<Bitmap, String> {
    let bad = || "immagine BMP non valida".to_string();
    if b.len() < 54 || &b[0..2] != b"BM" {
        return Err(bad());
    }
    let data_off = rd_u32(b, 10) as usize;
    let hsize = rd_u32(b, 14) as usize;
    if hsize < 40 || 14 + hsize > b.len() {
        return Err(bad());
    }
    let w = rd_u32(b, 18) as i32;
    let h = rd_u32(b, 22) as i32;
    let bpp = rd_u16(b, 28);
    let compression = rd_u32(b, 30);
    if w <= 0 || h == 0 || w > 65535 || h.unsigned_abs() > 65535 {
        return Err(bad());
    }
    let (width, height) = (w as usize, h.unsigned_abs() as usize);
    let top_down = h < 0;
    // Maschere dei canali: BI_BITFIELDS (3) le mette nell'intestazione V4/V5 o subito dopo quella da 40 byte.
    let (mut rm, mut gm, mut bm) = (0x00FF_0000u32, 0x0000_FF00u32, 0x0000_00FFu32);
    if bpp == 32 && compression == 3 {
        let mo = 14 + 40;
        if mo + 12 > b.len() {
            return Err(bad());
        }
        rm = rd_u32(b, mo);
        gm = rd_u32(b, mo + 4);
        bm = rd_u32(b, mo + 8);
    } else if compression != 0 {
        return Err(format!("BMP compresso (tipo {compression}) non supportato"));
    }
    let bytes_pp = match bpp {
        24 => 3,
        32 => 4,
        _ => return Err(format!("BMP a {bpp} bit non supportato")),
    };
    let stride = (width * bytes_pp + 3) & !3;
    if data_off + stride * height > b.len() {
        return Err(bad());
    }
    let mut pixels = vec![0u8; width * height * 4];
    let standard = bpp == 32 && rm == 0x00FF_0000 && gm == 0x0000_FF00 && bm == 0x0000_00FF;
    let shift = |m: u32| if m == 0 { 0 } else { m.trailing_zeros() };
    let (rs, gs, bs) = (shift(rm), shift(gm), shift(bm));
    for y in 0..height {
        let src_y = if top_down { y } else { height - 1 - y };
        let src = &b[data_off + src_y * stride..data_off + src_y * stride + width * bytes_pp];
        let dst = &mut pixels[y * width * 4..(y + 1) * width * 4];
        if standard {
            dst.copy_from_slice(src);
            for px in dst.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        } else if bpp == 32 {
            for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                let v = u32::from_le_bytes(*s);
                d[0] = ((v & bm) >> bs) as u8;
                d[1] = ((v & gm) >> gs) as u8;
                d[2] = ((v & rm) >> rs) as u8;
                d[3] = 255;
            }
        } else {
            for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<3>().0) {
                d[0] = s[0];
                d[1] = s[1];
                d[2] = s[2];
                d[3] = 255;
            }
        }
    }
    Ok(Bitmap { width: width as u32, height: height as u32, pixels })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bmp(width: i32, height: i32, bpp: u16, rows_bottom_up: &[Vec<u8>]) -> Vec<u8> {
        let bytes_pp = (bpp / 8) as usize;
        let stride = (width as usize * bytes_pp + 3) & !3;
        let mut v = Vec::new();
        v.extend_from_slice(b"BM");
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&54u32.to_le_bytes());
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&width.to_le_bytes());
        v.extend_from_slice(&height.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&bpp.to_le_bytes());
        v.extend_from_slice(&[0u8; 24]);
        for r in rows_bottom_up {
            let mut row = r.clone();
            row.resize(stride, 0);
            v.extend_from_slice(&row);
        }
        v
    }

    #[test]
    fn bmp_24_bit_dal_basso() {
        // 2×2: riga bassa blu/verde, riga alta rosso/bianco
        let data = bmp(2, 2, 24, &[vec![255, 0, 0, 0, 255, 0], vec![0, 0, 255, 255, 255, 255]]);
        let b = parse_bmp(&data).unwrap();
        assert_eq!((b.width, b.height), (2, 2));
        assert_eq!(b.pixel(0, 0), [0, 0, 255, 255]); // rosso in alto a sinistra
        assert_eq!(b.pixel(1, 0), [255, 255, 255, 255]);
        assert_eq!(b.pixel(0, 1), [255, 0, 0, 255]); // blu
        assert_eq!(b.pixel(1, 1), [0, 255, 0, 255]);
    }

    #[test]
    fn bmp_32_bit_dall_alto() {
        let data = bmp(1, -2, 32, &[vec![1, 2, 3, 0], vec![4, 5, 6, 0]]);
        let b = parse_bmp(&data).unwrap();
        assert_eq!(b.pixel(0, 0), [1, 2, 3, 255]);
        assert_eq!(b.pixel(0, 1), [4, 5, 6, 255]);
    }

    #[test]
    fn bmp_non_valido() {
        assert!(parse_bmp(b"BMxx").is_err());
        assert!(parse_bmp(&[0u8; 80]).is_err());
    }

    #[test]
    fn adattamento_alle_dimensioni_esatte() {
        let px = |w: u32, h: u32, v: u8| Bitmap { width: w, height: h, pixels: vec![v; (w * h * 4) as usize] };
        // differenza di arrotondamento: ritaglio / bianco
        let b = fit_exact(px(10, 9, 7), 9, 10);
        assert_eq!((b.width, b.height), (9, 10));
        assert_eq!(b.pixel(8, 8), [7, 7, 7, 7]);
        assert_eq!(b.pixel(0, 9), [255, 255, 255, 255]);
        // grande differenza: ridimensionata
        let b = fit_exact(px(100, 50, 3), 20, 10);
        assert_eq!((b.width, b.height, b.pixel(19, 9)[0]), (20, 10, 3));
        let b = fit_exact(px(4, 4, 1), 4, 4);
        assert_eq!(b.pixels.len(), 64);
    }

    #[test]
    fn rotazione_oraria() {
        // 2×1: [A B] → 1×2: [A; B] ruotata in senso orario diventa colonna con A in alto
        let b = Bitmap { width: 2, height: 1, pixels: vec![1, 1, 1, 1, 2, 2, 2, 2] };
        let r = b.rotated_cw();
        assert_eq!((r.width, r.height), (1, 2));
        assert_eq!(r.pixel(0, 0)[0], 1);
        assert_eq!(r.pixel(0, 1)[0], 2);
        // 2×2: [1 2; 3 4] → [3 1; 4 2]
        let b = Bitmap { width: 2, height: 2, pixels: [1u8, 2, 3, 4].iter().flat_map(|&v| [v; 4]).collect() };
        let r = b.rotated_cw();
        let got: Vec<u8> = (0..4).map(|i| r.pixel(i % 2, i / 2)[0]).collect();
        assert_eq!(got, vec![3, 1, 4, 2]);
    }
}
