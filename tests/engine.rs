//! Prove del motore PDF di Windows (Windows.Data.Pdf) e della stampa vera.

mod common;

use common::*;
use mypdf::render::{init_thread, LoadError, PdfDoc};

/// Pagina 200×100 pt: metà sinistra rossa (con il quarto in alto verde), metà destra blu.
fn colored_page(rotate: i64, crop: Option<[f32; 4]>) -> PageSpec {
    let mut ops = rect(1.0, 0.0, 0.0, 0.0, 0.0, 100.0, 100.0);
    ops.extend(rect(0.0, 1.0, 0.0, 0.0, 50.0, 100.0, 50.0));
    ops.extend(rect(0.0, 0.0, 1.0, 100.0, 0.0, 100.0, 100.0));
    PageSpec { media: (200.0, 100.0), crop, rotate, ops }
}

fn is_color(px: [u8; 4], rgb: (u8, u8, u8)) -> bool {
    let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
    (r - rgb.0 as i32).abs() < 40 && (g - rgb.1 as i32).abs() < 40 && (b - rgb.2 as i32).abs() < 40
}

const RED: (u8, u8, u8) = (255, 0, 0);
const GREEN: (u8, u8, u8) = (0, 255, 0);
const BLUE: (u8, u8, u8) = (0, 0, 255);

#[test]
fn disegna_pagina_intera() {
    init_thread();
    let pdf = bytes(build(vec![colored_page(0, None)]));
    let doc = PdfDoc::load(&pdf, None).unwrap();
    assert_eq!(doc.page_count(), 1);
    let (w, h) = doc.page_size(0);
    // 200 pt = 200/72 pollici = 266,7 DIP
    assert!((w - 266.67).abs() < 0.5 && (h - 133.33).abs() < 0.5, "{w}×{h}");
    let bmp = doc.render(0, None, 400, 200).unwrap();
    assert_eq!((bmp.width, bmp.height), (400, 200));
    assert!(is_color(bmp.pixel(50, 150), RED), "{:?}", bmp.pixel(50, 150));
    assert!(is_color(bmp.pixel(50, 50), GREEN), "{:?}", bmp.pixel(50, 50));
    assert!(is_color(bmp.pixel(300, 100), BLUE), "{:?}", bmp.pixel(300, 100));
}

#[test]
fn disegna_un_riquadro() {
    init_thread();
    let pdf = bytes(build(vec![colored_page(0, None)]));
    let doc = PdfDoc::load(&pdf, None).unwrap();
    let (w, h) = doc.page_size(0);
    // metà destra: tutta blu
    let right = doc.render(0, Some((w / 2.0, 0.0, w / 2.0, h)), 100, 100).unwrap();
    for (x, y) in [(5, 5), (50, 50), (94, 94)] {
        assert!(is_color(right.pixel(x, y), BLUE), "({x},{y}) {:?}", right.pixel(x, y));
    }
    // quarto in alto a sinistra: verde
    let tl = doc.render(0, Some((0.0, 0.0, w / 2.0, h / 2.0)), 100, 50).unwrap();
    for (x, y) in [(5, 5), (50, 25), (94, 44)] {
        assert!(is_color(tl.pixel(x, y), GREEN), "({x},{y}) {:?}", tl.pixel(x, y));
    }
    // quarto in basso a sinistra: rosso
    let bl = doc.render(0, Some((0.0, h / 2.0, w / 2.0, h / 2.0)), 100, 50).unwrap();
    assert!(is_color(bl.pixel(50, 25), RED), "{:?}", bl.pixel(50, 25));
}

#[test]
fn pagina_ruotata_e_ritagliata() {
    init_thread();
    let pdf = bytes(build(vec![colored_page(90, None), colored_page(0, Some([100.0, 0.0, 200.0, 100.0]))]));
    let doc = PdfDoc::load(&pdf, None).unwrap();
    let (w, h) = doc.page_size(0);
    assert!((w - 133.33).abs() < 0.5 && (h - 266.67).abs() < 0.5, "ruotata: {w}×{h}");
    // ruotata di 90° in senso orario: la metà sinistra (rossa/verde) va in alto
    let bmp = doc.render(0, None, 100, 200).unwrap();
    assert!(!is_color(bmp.pixel(50, 50), BLUE));
    assert!(is_color(bmp.pixel(50, 150), BLUE), "{:?}", bmp.pixel(50, 150));
    // CropBox sulla metà destra: solo blu, pagina quadrata
    let (w, h) = doc.page_size(1);
    assert!((w - 133.33).abs() < 0.5 && (h - 133.33).abs() < 0.5, "ritagliata: {w}×{h}");
    let bmp = doc.render(1, None, 100, 100).unwrap();
    assert!(is_color(bmp.pixel(10, 10), BLUE) && is_color(bmp.pixel(90, 90), BLUE));
}

#[test]
fn pdf_non_valido_e_protetto() {
    init_thread();
    assert!(matches!(PdfDoc::load(b"non sono un pdf", None), Err(LoadError::Other(_))));

    use lopdf::{Document, EncryptionState, EncryptionVersion, Permissions};
    let mut d = Document::load_mem(&labeled(&["Riservato"])).unwrap();
    let id = lopdf::Object::String(b"0123456789abcdef".to_vec(), lopdf::StringFormat::Hexadecimal);
    d.trailer.set("ID", vec![id.clone(), id]);
    let v =EncryptionVersion::V2 { document: &d, owner_password: "padrone", user_password: "1234", key_length: 128, permissions: Permissions::all() };
    let state = EncryptionState::try_from(v).unwrap();
    d.encrypt(&state).unwrap();
    let locked = bytes(d);
    assert_eq!(PdfDoc::load(&locked, None).err(), Some(LoadError::PasswordRequired));
    assert_eq!(PdfDoc::load(&locked, Some("sbagliata")).err(), Some(LoadError::PasswordRequired));
    let doc = PdfDoc::load(&locked, Some("1234")).unwrap();
    assert_eq!(doc.page_count(), 1);
    assert!(doc.render(0, None, 60, 85).is_ok());
}

#[test]
fn file_troncati_danneggiati_e_non_pdf() {
    use mypdf::render::open_document;
    init_thread();
    let labels: Vec<String> = (1..=8).map(|i| format!("Pagina {i}")).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let full = labeled(&refs);
    let o = open_document(&full, None).unwrap();
    assert!(!o.damaged);
    assert_eq!(o.doc.page_count(), 8);

    // troncato in molti punti: mai un panico; se resta qualcosa si vede, segnato come danneggiato
    let mut recovered_any = 0;
    for pct in (2..100).step_by(7) {
        let cut = &full[..full.len() * pct / 100];
        match open_document(cut, None) {
            Ok(o) => {
                assert!(o.damaged, "{pct}%: un file troncato va segnato come danneggiato");
                assert!(o.doc.page_count() >= 1 && o.doc.page_count() <= 8, "{pct}%");
                assert!(o.doc.render(0, None, 60, 85).is_ok(), "{pct}%");
                recovered_any += 1;
            }
            Err(LoadError::Broken(_)) => {}
            Err(e) => panic!("{pct}%: errore inatteso {e:?}"),
        }
    }
    assert!(recovered_any >= 8, "recuperati solo {recovered_any} tagli");
    let o = open_document(&full[..full.len() * 3 / 4], None).unwrap();
    assert!(o.damaged && o.doc.page_count() >= 4, "al 75% restano almeno 4 pagine: {}", o.doc.page_count());

    // byte rovinati qua e là: si apre (magari a pezzi) o si dice che è danneggiato
    let mut x = 7u32;
    for round in 0..12 {
        let mut bad = full.clone();
        for _ in 0..(1 + round * 3) {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let pos = 10 + (x as usize % (bad.len() - 10));
            bad[pos] = (x >> 24) as u8;
        }
        match open_document(&bad, None) {
            Ok(o) => {
                for p in 0..o.doc.page_count() {
                    let _ = o.doc.render(p, None, 40, 56); // può fallire, ma non chiudere tutto
                }
            }
            Err(LoadError::Broken(_)) | Err(LoadError::Other(_)) => {}
            Err(e) => panic!("giro {round}: errore inatteso {e:?}"),
        }
    }

    // altri formati
    for other in [&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..], b"PK\x03\x04 zip", b"Testo semplice, non un PDF", b"", b"\xff\xd8\xff\xe0 jpeg"] {
        assert_eq!(open_document(other, None).err(), Some(LoadError::NotPdf));
    }
    // solo l'intestazione, o spazzatura dopo l'intestazione: PDF danneggiato
    assert!(matches!(open_document(b"%PDF-1.7\n", None), Err(LoadError::Broken(_))));
    let mut junk = b"%PDF-1.4\n".to_vec();
    junk.extend((0..50_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8));
    assert!(matches!(open_document(&junk, None), Err(LoadError::Broken(_))));
}

#[test]
fn molte_pagine_e_pagina_inesistente() {
    init_thread();
    let labels: Vec<String> = (1..=40).map(|i| format!("P{i}")).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let doc = PdfDoc::load(&labeled(&refs), None).unwrap();
    assert_eq!(doc.page_count(), 40);
    assert!(doc.render(39, None, 50, 70).is_ok());
    assert!(doc.render(40, None, 50, 70).is_err());
}

/// Stampa vera su "Microsoft Print to PDF", mandata su file: si controlla il PDF prodotto.
#[test]
fn stampa_su_microsoft_print_to_pdf() {
    const PRINTER: &str = "Microsoft Print to PDF";
    let Ok(dc) = mypdf::print::open_printer(Some(PRINTER)) else {
        eprintln!("stampante \"{PRINTER}\" assente: prova saltata");
        return;
    };
    let dir = TempDir::new("print");
    let out = dir.path("stampato.pdf");
    // tre pagine, una orizzontale (va ruotata sul foglio)
    let mut pages: Vec<PageSpec> = ["Uno", "Due"].iter().map(|l| PageSpec { media: (595.0, 842.0), crop: None, rotate: 0, ops: text(l) }).collect();
    pages.push(colored_page(0, None));
    let pdf = bytes(build(pages));
    let mut progress = Vec::new();
    let n = mypdf::print::print_document(&dc, &pdf, None, "prova myPDF", &[], Some(&out), &mut |d, t| progress.push((d, t))).unwrap();
    assert_eq!(n, 3);
    assert_eq!(progress, vec![(1, 3), (2, 3), (3, 3)]);
    drop(dc);
    // Lo spooler scrive il file poco dopo: si aspetta che sia completo.
    let start = std::time::Instant::now();
    let pages = loop {
        if let Ok(bytes) = std::fs::read(&out) {
            if let Ok(doc) = lopdf::Document::load_mem(&bytes) {
                break doc.get_pages().len();
            }
        }
        assert!(start.elapsed().as_secs() < 60, "il PDF stampato non è arrivato");
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    assert_eq!(pages, 3);
}
