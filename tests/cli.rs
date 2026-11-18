//! Prove della riga di comando sull'eseguibile vero (senza aprire l'interfaccia).

mod common;

use common::*;
use std::process::Command;

fn mypdf() -> Command {
    Command::new(env!("CARGO_BIN_EXE_myPDF"))
}

const PDF_PRINTER: &str = "Microsoft Print to PDF";

fn has_pdf_printer() -> bool {
    mypdf::print::list_printers().iter().any(|p| p == PDF_PRINTER)
}

/// Aspetta che lo spooler finisca di scrivere il PDF stampato e ne restituisce le pagine.
fn wait_printed_pages(path: &std::path::Path) -> usize {
    let start = std::time::Instant::now();
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(doc) = lopdf::Document::load_mem(&bytes) {
                return doc.get_pages().len();
            }
        }
        assert!(start.elapsed().as_secs() < 60, "il PDF stampato non è arrivato");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[test]
fn elenco_stampanti() {
    let o = mypdf().arg("--printers").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("Stampanti installate"), "{text}");
    if has_pdf_printer() {
        assert!(text.contains(PDF_PRINTER), "{text}");
    }
}

#[test]
fn stampa_diretta_da_riga_di_comando() {
    if !has_pdf_printer() {
        eprintln!("stampante \"{PDF_PRINTER}\" assente: prova saltata");
        return;
    }
    let dir = TempDir::new("cli-print");
    let src = dir.path("da stampare.pdf");
    std::fs::write(&src, labeled(&["P1", "P2", "P3"])).unwrap();
    // pagine 1 e 3, due copie fascicolate: 4 pagine stampate
    let out = dir.path("stampato.pdf");
    let o = mypdf()
        .arg("--print")
        .arg(&src)
        .args(["--printer", PDF_PRINTER, "--pages", "1,3", "--copies", "2", "--to-file"])
        .arg(&out)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{stdout}\n{}", String::from_utf8_lossy(&o.stderr));
    assert!(stdout.contains("4 pagine (2 copie)"), "{stdout}");
    assert_eq!(wait_printed_pages(&out), 4);
    // pagine fuori dal documento: parametri sbagliati, niente stampa
    let o = mypdf().arg("--print").arg(&src).args(["--printer", PDF_PRINTER, "--pages", "9"]).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("fuori dal documento"));
}

#[test]
fn stampante_inesistente() {
    let dir = TempDir::new("cli-print-err");
    let src = dir.path("a.pdf");
    std::fs::write(&src, labeled(&["A"])).unwrap();
    let o = mypdf().arg("--print").arg(&src).args(["--printer", "Stampante che non esiste 123"]).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--printers"));
    let o = mypdf().arg("--print").arg(dir.path("manca.pdf")).args(["--printer", PDF_PRINTER]).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn unisce_da_riga_di_comando() {
    let dir = TempDir::new("cli");
    std::fs::write(dir.path("a.pdf"), labeled(&["A1", "A2"])).unwrap();
    std::fs::write(dir.path("b.pdf"), labeled(&["B1"])).unwrap();
    let out = dir.path("unito.pdf");
    let o = mypdf().arg("--merge").arg(&out).arg(dir.path("a.pdf")).arg(dir.path("b.pdf")).output().unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "stdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&o.stderr));
    assert!(stdout.contains("Fatto: 3 pagine"), "{stdout}");
    assert_eq!(page_texts(&std::fs::read(&out).unwrap()), vec!["A1", "A2", "B1"]);

    // L'uscita esiste già: senza -y si ferma, con -y sovrascrive (qui in ordine inverso).
    let o = mypdf().args(["-m"]).arg(&out).arg(dir.path("b.pdf")).arg(dir.path("a.pdf")).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("esiste già"));
    let o = mypdf().args(["-m", "-y", "-q"]).arg(&out).arg(dir.path("b.pdf")).arg(dir.path("a.pdf")).output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(o.stdout.iter().all(|c| c.is_ascii_whitespace()), "con -q nessun messaggio");
    assert_eq!(page_texts(&std::fs::read(&out).unwrap()), vec!["B1", "A1", "A2"]);
}

#[test]
fn caratteri_jolly_e_cartelle() {
    let dir = TempDir::new("cli-jolly");
    for (name, label) in [("s10.pdf", "DIECI"), ("s2.pdf", "DUE"), ("s1.pdf", "UNO")] {
        std::fs::write(dir.path(name), labeled(&[label])).unwrap();
    }
    // uscita nella stessa cartella e compresa da *.pdf: non deve unire se stessa
    let out = dir.path("tutti.pdf");
    std::fs::write(&out, labeled(&["VECCHIO"])).unwrap();
    let o = mypdf().args(["--unisci", "-y", "-o"]).arg(&out).arg(dir.0.join("*.pdf")).output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(page_texts(&std::fs::read(&out).unwrap()), vec!["UNO", "DUE", "DIECI"]);
    // una cartella = i suoi PDF in ordine naturale
    let out2 = std::env::temp_dir().join(format!("mypdf-cartella-{}.pdf", std::process::id()));
    let o = mypdf().arg("-m").arg(&out2).arg(&dir.0).arg("-y").output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(page_texts(&std::fs::read(&out2).unwrap()), vec!["UNO", "DUE", "DIECI", "UNO", "DUE", "DIECI"]);
    let _ = std::fs::remove_file(&out2);
}

#[test]
fn errori_e_aiuto() {
    let dir = TempDir::new("cli-err");
    std::fs::write(dir.path("a.pdf"), labeled(&["A"])).unwrap();
    std::fs::write(dir.path("rotto.pdf"), b"non un pdf").unwrap();
    // file che non esiste
    let o = mypdf().arg("--merge").arg(dir.path("x.pdf")).arg(dir.path("a.pdf")).arg(dir.path("manca.pdf")).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("non trovato"));
    assert!(!dir.path("x.pdf").exists());
    // file non valido: errore col suo nome, nessuna uscita
    let o = mypdf().arg("--merge").arg(dir.path("x.pdf")).arg(dir.path("a.pdf")).arg(dir.path("rotto.pdf")).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("rotto.pdf") && err.contains("non è un file PDF"), "{err}");
    assert!(!dir.path("x.pdf").exists());
    // PDF troncato: unito con le pagine leggibili e un avviso
    let full = labeled(&["T1", "T2", "T3", "T4"]);
    std::fs::write(dir.path("troncato.pdf"), &full[..full.len() * 3 / 4]).unwrap();
    let o = mypdf().arg("--merge").arg(dir.path("x.pdf")).arg(dir.path("a.pdf")).arg(dir.path("troncato.pdf")).output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("danneggiato"));
    let texts = page_texts(&std::fs::read(dir.path("x.pdf")).unwrap());
    assert_eq!(&texts[..2], &["A".to_string(), "T1".to_string()]);
    // parametri sbagliati
    let o = mypdf().args(["--merge", "solo.pdf"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    // aiuto e versione
    let o = mypdf().arg("--help").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stdout).contains("--merge"));
    let o = mypdf().arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains(env!("CARGO_PKG_VERSION")));
}
