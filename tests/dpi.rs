//! Il motore PDF di Windows, in un processo "DPI aware" (come l'interfaccia), moltiplica le
//! dimensioni chieste per la scala dello schermo: render() deve comunque restituire
//! esattamente i pixel chiesti. Prova in un eseguibile a parte, perché la consapevolezza DPI
//! vale per tutto il processo.

mod common;

use common::*;
use mypdf::render::{init_thread, PdfDoc};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

#[test]
fn dimensioni_esatte_anche_con_processo_dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let dpi = unsafe { GetDpiForSystem() };
    init_thread();
    let pdf = bytes(build(vec![PageSpec { media: (200.0, 100.0), crop: None, rotate: 0, ops: rect(0.0, 0.0, 1.0, 0.0, 0.0, 200.0, 100.0) }]));
    let doc = PdfDoc::load(&pdf, None).unwrap();
    for (w, h) in [(400, 200), (1219, 609), (37, 19)] {
        let b = doc.render(0, None, w, h).unwrap();
        assert_eq!((b.width, b.height), (w, h), "pagina intera a {dpi} dpi");
        let (pw, ph) = doc.page_size(0);
        let t = doc.render(0, Some((0.0, 0.0, pw / 2.0, ph)), w / 2, h).unwrap();
        assert_eq!((t.width, t.height), (w / 2, h), "riquadro a {dpi} dpi");
        // tutta blu fino all'ultimo pixel
        let px = b.pixel(w - 1, h - 1);
        assert!(px[0] > 200 && px[2] < 60, "{px:?}");
    }
}
