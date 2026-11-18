//! Riga di comando: unione senza interfaccia, stampa, registrazione per i `.pdf`.
//!
//! myPDF è un programma "a finestre" (così aprendo un PDF da Esplora file non compare una
//! console): per i comandi senza interfaccia si aggancia alla console da cui è stato
//! lanciato, oppure scrive sull'uscita rediretta.

use crate::files::expand_arg;
use crate::merge::{merge_files, MergeOptions};
use crate::print::{
    default_printer, list_printers, open_printer, parse_page_ranges, print_dialog, print_document, DialogOptions, PrinterDc, PrinterSettings,
};
use crate::render::{init_thread, open_document, LoadError, PdfDoc};
use crate::wide::WStr;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use windows::core::w;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, HWND, RECT};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Console::{AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, MessageBoxW, SystemParametersInfoW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, SPI_GETWORKAREA,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WS_EX_TOOLWINDOW, WS_POPUP,
};

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CANCELLED: i32 = 3;

#[derive(Debug, PartialEq)]
pub struct MergeArgs {
    pub output: PathBuf,
    pub inputs: Vec<String>,
    pub overwrite: bool,
    pub bookmarks: bool,
    pub passwords: Vec<String>,
    pub quiet: bool,
}

#[derive(Debug, PartialEq)]
pub struct PrintArgs {
    /// File da stampare (anche cartelle e nomi con * e ?), ognuno come lavoro di stampa a sé.
    pub files: Vec<String>,
    /// Stampante; senza (e senza `default_printer`) si apre la finestra di stampa di Windows.
    pub printer: Option<String>,
    /// Stampa sulla stampante predefinita senza aprire la finestra di stampa.
    pub default_printer: bool,
    pub pages: Option<String>,
    pub copies: u32,
    /// Stampa su file (per stampanti come "Microsoft Print to PDF").
    pub output: Option<PathBuf>,
    pub passwords: Vec<String>,
    pub quiet: bool,
}

impl PrintArgs {
    fn one(file: &str, printer: Option<String>) -> PrintArgs {
        PrintArgs {
            files: vec![file.to_string()],
            printer,
            default_printer: false,
            pages: None,
            copies: 1,
            output: None,
            passwords: Vec::new(),
            quiet: false,
        }
    }

    /// Vero se la stampante va scelta nella finestra di stampa di Windows.
    pub fn uses_dialog(&self) -> bool {
        self.printer.is_none() && !self.default_printer
    }
}

#[derive(Debug, PartialEq)]
pub enum Command {
    Gui(Vec<PathBuf>),
    Merge(MergeArgs),
    Print(PrintArgs),
    Printers,
    Register,
    Unregister,
    Help,
    Version,
}

fn is_flag(a: &str, names: &[&str]) -> bool {
    names.iter().any(|n| a.eq_ignore_ascii_case(n))
}

const MERGE_FLAGS: [&str; 6] = ["-m", "--merge", "/merge", "--unisci", "/unisci", "-merge"];
const PRINT_FLAGS: [&str; 5] = ["/p", "--print", "/print", "--stampa", "-print"];

/// Interpreta gli argomenti (senza il nome del programma).
pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some(first) = args.first() else { return Ok(Command::Gui(Vec::new())) };
    let f = first.as_str();
    if is_flag(f, &["-h", "--help", "/?", "-?", "/h", "/help", "--aiuto"]) {
        return Ok(Command::Help);
    }
    if is_flag(f, &["-v", "--version", "/v", "/version", "--versione"]) {
        return Ok(Command::Version);
    }
    if is_flag(f, &["--register", "/register", "--registra"]) {
        return Ok(Command::Register);
    }
    if is_flag(f, &["--unregister", "/unregister", "--deregistra"]) {
        return Ok(Command::Unregister);
    }
    if is_flag(f, &MERGE_FLAGS) {
        return parse_merge(&args[1..]);
    }
    if is_flag(f, &PRINT_FLAGS) {
        return parse_print(&args[1..]);
    }
    if is_flag(f, &["/pt", "--printto"]) {
        // verbo "Stampa su" di Esplora file: file, stampante, driver, porta
        let file = args.get(1).ok_or("manca il file da stampare")?;
        let printer = args.get(2).ok_or("manca il nome della stampante")?;
        return Ok(Command::Print(PrintArgs::one(file, Some(printer.clone()))));
    }
    if is_flag(f, &["--printers", "/printers", "--stampanti"]) {
        return Ok(Command::Printers);
    }
    if let Some(bad) = args.iter().find(|a| a.starts_with("--")) {
        return Err(format!("opzione sconosciuta: {bad}"));
    }
    Ok(Command::Gui(args.iter().map(PathBuf::from).collect()))
}

fn take_value<'a>(rest: &'a [String], i: &mut usize, what: &str) -> Result<&'a String, String> {
    *i += 1;
    rest.get(*i).ok_or_else(|| format!("manca {what} dopo {}", rest[*i - 1]))
}

fn parse_merge(rest: &[String]) -> Result<Command, String> {
    let mut output: Option<PathBuf> = None;
    let mut positionals = Vec::new();
    let (mut overwrite, mut bookmarks, mut quiet) = (false, true, false);
    let mut passwords = Vec::new();
    let mut only_files = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if only_files {
            positionals.push(a.to_string());
        } else if a == "--" {
            only_files = true;
        } else if is_flag(a, &["-o", "--output", "--out", "--uscita", "/o"]) {
            output = Some(take_value(rest, &mut i, "il nome del file di uscita")?.into());
        } else if is_flag(a, &["-y", "--overwrite", "--sovrascrivi", "-f", "--force", "/y"]) {
            overwrite = true;
        } else if is_flag(a, &["--no-bookmarks", "--senza-segnalibri"]) {
            bookmarks = false;
        } else if is_flag(a, &["--password", "-p", "/password"]) {
            passwords.push(take_value(rest, &mut i, "la password")?.clone());
        } else if is_flag(a, &["-q", "--quiet", "--silenzioso", "/q"]) {
            quiet = true;
        } else if a.starts_with("--") || (a.starts_with('-') && a.len() == 2) {
            return Err(format!("opzione sconosciuta per --merge: {a}"));
        } else {
            positionals.push(a.to_string());
        }
        i += 1;
    }
    let output = match output {
        Some(o) => o,
        None => {
            if positionals.is_empty() {
                return Err("indica il file di uscita e i PDF da unire".into());
            }
            PathBuf::from(positionals.remove(0))
        }
    };
    if positionals.is_empty() {
        return Err("indica almeno un PDF da unire".into());
    }
    let output = if output.extension().is_none() { output.with_extension("pdf") } else { output };
    Ok(Command::Merge(MergeArgs { output, inputs: positionals, overwrite, bookmarks, passwords, quiet }))
}

fn parse_print(rest: &[String]) -> Result<Command, String> {
    let mut p = PrintArgs { files: Vec::new(), ..PrintArgs::one("", None) };
    let mut only_files = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if only_files {
            p.files.push(a.to_string());
        } else if a == "--" {
            only_files = true;
        } else if is_flag(a, &["--printer", "--stampante", "/printer", "-d"]) {
            p.printer = Some(take_value(rest, &mut i, "il nome della stampante")?.clone());
        } else if is_flag(a, &["--default-printer", "--predefinita", "/default"]) {
            p.default_printer = true;
        } else if is_flag(a, &["--pages", "--pagine", "/pages"]) {
            p.pages = Some(take_value(rest, &mut i, "l'elenco delle pagine")?.clone());
        } else if is_flag(a, &["--copies", "--copie", "-n", "/copies"]) {
            let v = take_value(rest, &mut i, "il numero di copie")?;
            p.copies = v.parse::<u32>().ok().filter(|n| (1..=999).contains(n)).ok_or_else(|| format!("numero di copie non valido: {v} (da 1 a 999)"))?;
        } else if is_flag(a, &["--to-file", "--su-file"]) {
            p.output = Some(PathBuf::from(take_value(rest, &mut i, "il nome del file")?));
        } else if is_flag(a, &["--password", "-p"]) {
            p.passwords.push(take_value(rest, &mut i, "la password")?.clone());
        } else if is_flag(a, &["-q", "--quiet", "--silenzioso", "/q"]) {
            p.quiet = true;
        } else if a.starts_with("--") || (a.starts_with('-') && a.len() == 2) {
            return Err(format!("opzione sconosciuta per --print: {a}"));
        } else {
            p.files.push(a.to_string());
        }
        i += 1;
    }
    if p.files.is_empty() {
        return Err("manca il file da stampare".into());
    }
    Ok(Command::Print(p))
}

pub fn usage() -> String {
    format!(
        "myPDF {} - visualizzatore e unione di PDF per Windows

Uso:
  myPDF.exe [file.pdf ...]
      Apre i file nell'interfaccia grafica.

  myPDF.exe --merge <uscita.pdf> <file1.pdf> <file2.pdf> [...]
      Unisce i PDF nell'ordine dato, senza aprire l'interfaccia.
      Come ingressi vanno bene anche cartelle (i PDF che contengono)
      e nomi con * e ? (es. C:\\Scansioni\\*.pdf), in ordine naturale.
      Opzioni:
        -o, --output <file>   file di uscita (al posto del primo nome)
        -y, --overwrite       sovrascrive il file di uscita se esiste
        --no-bookmarks        senza un segnalibro per ogni file unito
        --password <pw>       password dei PDF protetti (anche più volte)
        -q, --quiet           nessun messaggio se va tutto bene
      Sinonimi di --merge: -m, /merge, --unisci

  myPDF.exe --print <file.pdf> [altri file...] [opzioni]
      Stampa senza aprire il visualizzatore, ogni file come lavoro a sé
      (anche cartelle e nomi con * e ?). Se la stampante non è indicata si
      apre la finestra di stampa di Windows per sceglierla (con pagine e
      copie). Le pagine più grandi del foglio vengono ridotte e quelle
      orizzontali ruotate.
      Opzioni:
        --printer \"<nome>\"    stampa subito su questa stampante
        --default-printer     stampa subito sulla stampante predefinita
        --pages 1-3,5,8-      solo queste pagine
        --copies <n>          numero di copie, fascicolate
        --password <pw>       password dei PDF protetti
        -q, --quiet           nessun messaggio se va tutto bene
      Sinonimi di --print: /p, --stampa

  myPDF.exe --printers
      Elenca le stampanti installate (* = predefinita).

  myPDF.exe --register   |   myPDF.exe --unregister
      Aggiunge (o toglie) myPDF tra i programmi che aprono i PDF.

Codici di uscita: 0 riuscito, 1 errore, 2 parametri sbagliati,
3 stampa annullata nella finestra di stampa.

myPDF è un programma a finestre: nel Prompt dei comandi usa
  start /wait myPDF.exe --merge ...
per aspettarne la fine (nei file .bat si aspetta da sé); in PowerShell
  myPDF.exe --merge ... | Out-Host",
        crate::VERSION
    )
}

/// Collega l'uscita alla console da cui il programma è stato lanciato. Falso se non c'è.
fn attach_console() -> bool {
    unsafe {
        let valid = |h: windows::core::Result<HANDLE>| h.is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
        if valid(GetStdHandle(STD_OUTPUT_HANDLE)) {
            return true; // uscita rediretta su file o pipe
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return false;
        }
        if let Ok(h) = CreateFileW(
            w!("CONOUT$"),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
            if !valid(GetStdHandle(STD_ERROR_HANDLE)) {
                let _ = SetStdHandle(STD_ERROR_HANDLE, h);
            }
        }
        // il prompt è già stato scritto: si comincia su una riga nuova
        println!();
        true
    }
}

fn message_box(text: &str, error: bool) {
    let t = WStr::new(text);
    let c = WStr::new(crate::APP_NAME);
    unsafe {
        MessageBoxW(None, t.pcwstr(), c.pcwstr(), MB_OK | if error { MB_ICONERROR } else { MB_ICONINFORMATION });
    }
}

/// Uscita dei comandi: sulla console se c'è, altrimenti (solo errori e aiuto) in una finestra.
struct Out {
    console: bool,
}

impl Out {
    fn info(&self, text: &str) {
        if self.console {
            println!("{text}");
            let _ = std::io::stdout().flush();
        }
    }
    fn error(&self, text: &str) {
        if self.console {
            eprintln!("Errore: {text}");
            let _ = std::io::stderr().flush();
        } else {
            message_box(&format!("Errore: {text}"), true);
        }
    }
    fn always(&self, text: &str) {
        if self.console {
            println!("{text}");
            let _ = std::io::stdout().flush();
        } else {
            message_box(text, false);
        }
    }
}

/// Punto d'ingresso del programma: interfaccia o comando.
pub fn main(args: Vec<OsString>) -> i32 {
    let args: Vec<String> = args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
    match parse(&args) {
        Ok(Command::Gui(files)) => crate::gui::run(files),
        Ok(cmd) => {
            let out = Out { console: attach_console() };
            run(cmd, &out)
        }
        Err(e) => {
            let out = Out { console: attach_console() };
            out.error(&e);
            out.info("Usa --help per l'elenco dei comandi.");
            EXIT_USAGE
        }
    }
}

fn run(cmd: Command, out: &Out) -> i32 {
    match cmd {
        Command::Gui(_) => unreachable!(),
        Command::Help => {
            out.always(&usage());
            EXIT_OK
        }
        Command::Version => {
            out.always(&format!("myPDF {}", crate::VERSION));
            EXIT_OK
        }
        Command::Register | Command::Unregister => {
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => {
                    out.error(&e.to_string());
                    return EXIT_ERROR;
                }
            };
            let register = cmd == Command::Register;
            let r = if register { crate::assoc::register(&exe) } else { crate::assoc::unregister(&exe) };
            match r {
                Ok(()) if register => {
                    out.always("myPDF è registrato per i file PDF.\nPer renderlo predefinito sceglilo in Impostazioni > App > App predefinite.");
                    EXIT_OK
                }
                Ok(()) => {
                    out.always("Registrazione di myPDF per i file PDF tolta.");
                    EXIT_OK
                }
                Err(e) => {
                    out.error(&e);
                    EXIT_ERROR
                }
            }
        }
        Command::Merge(m) => run_merge(m, out),
        Command::Print(p) => run_print(p, out),
        Command::Printers => run_printers(out),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Elenco dei file da unire: caratteri jolly e cartelle espansi, senza il file di uscita
/// (che altrimenti finirebbe dentro se stesso con `*.pdf`).
pub fn resolve_inputs(inputs: &[String], output: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for a in inputs {
        let expanded = expand_arg(a)?;
        let pattern = expanded.len() > 1 || Path::new(a).is_dir() || a.contains(['*', '?']);
        for p in expanded {
            if pattern && same_file(&p, output) {
                continue;
            }
            files.push(p);
        }
    }
    if files.is_empty() {
        return Err("nessun PDF da unire".into());
    }
    Ok(files)
}

fn run_merge(m: MergeArgs, out: &Out) -> i32 {
    if m.output.is_dir() {
        out.error(&format!("{} è una cartella: indica il nome del file PDF da creare", m.output.display()));
        return EXIT_USAGE;
    }
    if m.output.exists() && !m.overwrite {
        out.error(&format!("{} esiste già: aggiungi -y per sovrascriverlo", m.output.display()));
        return EXIT_ERROR;
    }
    let files = match resolve_inputs(&m.inputs, &m.output) {
        Ok(f) => f,
        Err(e) => {
            out.error(&e);
            return EXIT_ERROR;
        }
    };
    if !m.quiet {
        out.info(&format!("Unione di {} file in {}:", files.len(), m.output.display()));
        for (i, f) in files.iter().enumerate() {
            out.info(&format!("  {:>3}. {}", i + 1, f.display()));
        }
    }
    let opts = MergeOptions { bookmarks: m.bookmarks, passwords: m.passwords };
    match merge_files(&files, &m.output, &opts) {
        Ok(r) => {
            // avviso anche con -q: il risultato non contiene tutto
            for f in &r.repaired {
                eprintln!("Attenzione: {} è danneggiato, sono state unite solo le pagine leggibili", f.display());
            }
            if !m.quiet {
                out.info(&format!("Fatto: {} pagine in {}", r.pages, m.output.display()));
            }
            EXIT_OK
        }
        Err(e) => {
            out.error(&e.to_string());
            EXIT_ERROR
        }
    }
}

fn run_printers(out: &Out) -> i32 {
    let default = default_printer();
    let printers = list_printers();
    if printers.is_empty() {
        out.always("Nessuna stampante installata.");
        return EXIT_OK;
    }
    let mut text = String::from("Stampanti installate (* = predefinita):");
    for name in printers {
        let mark = if default.as_deref() == Some(name.as_str()) { "*" } else { " " };
        text.push_str(&format!("\n {mark} {name}"));
    }
    out.always(&text);
    EXIT_OK
}

fn run_print(p: PrintArgs, out: &Out) -> i32 {
    let mut files = Vec::new();
    for a in &p.files {
        match expand_arg(a) {
            Ok(v) => files.extend(v),
            Err(e) => {
                out.error(&e);
                return EXIT_ERROR;
            }
        }
    }
    if p.output.is_some() && files.len() > 1 {
        out.error("--to-file vale per un solo file da stampare");
        return EXIT_USAGE;
    }
    // Stampante: scelta nella finestra di Windows, oppure quella indicata o la predefinita.
    let (dc, printer_name, chosen_pages, copies) = if p.uses_dialog() {
        match choose_in_dialog(&files, &p) {
            // le copie scelte nella finestra le fa il driver della stampante
            Ok(Some((dc, name, pages))) => (dc, name, pages, 1),
            Ok(None) => {
                if !p.quiet {
                    out.info("Stampa annullata.");
                }
                return EXIT_CANCELLED;
            }
            Err(e) => {
                out.error(&e);
                return EXIT_ERROR;
            }
        }
    } else {
        let name = match p.printer.clone().or_else(default_printer) {
            Some(n) => n,
            None => {
                out.error("nessuna stampante predefinita: indicane una con --printer (elenco con --printers)");
                return EXIT_ERROR;
            }
        };
        match open_printer(Some(&name)) {
            Ok(dc) => (dc, name, None, p.copies),
            Err(e) => {
                out.error(&e);
                return EXIT_ERROR;
            }
        }
    };
    // Le pagine si disegnano su un thread a parte: questo resta libero per la finestra di
    // stampa (che vuole un thread STA) e con la sua scala DPI.
    let (files_ref, p_ref, pages_ref) = (&files, &p, chosen_pages.as_deref());
    let results = std::thread::scope(|s| {
        s.spawn(move || {
            init_thread();
            files_ref.iter().map(|f| print_one(&dc, f, p_ref, pages_ref, copies)).collect::<Vec<_>>()
        })
        .join()
        .unwrap_or_else(|_| files_ref.iter().map(|_| Err("errore interno durante la stampa".to_string())).collect())
    });
    let mut failed = 0;
    for (file, r) in files.iter().zip(results) {
        match r {
            Ok(n) => {
                if !p.quiet {
                    let copies = if copies > 1 { format!(" ({copies} copie)") } else { String::new() };
                    out.info(&format!("Stampato {}: {n} pagine{copies} su \"{printer_name}\"", file.display()));
                }
            }
            Err(e) => {
                out.error(&format!("{}: {e}", file.display()));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        EXIT_ERROR
    } else {
        EXIT_OK
    }
}

/// Apre un PDF provando nessuna password e poi quelle date; restituisce anche quella giusta.
fn open_with_passwords(bytes: &[u8], passwords: &[String]) -> Result<(PdfDoc, Option<String>), String> {
    match open_document(bytes, None) {
        Ok(o) => Ok((o.doc, None)),
        Err(LoadError::PasswordRequired) => {
            for pw in passwords {
                if let Ok(o) = open_document(bytes, Some(pw)) {
                    return Ok((o.doc, Some(pw.clone())));
                }
            }
            Err(if passwords.is_empty() { "il PDF è protetto: indica la password con --password".into() } else { "nessuna delle password date apre il PDF".into() })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Finestra di stampa di Windows. Con un solo file permette di scegliere le pagine (già
/// impostate da --pages); con più file solo stampante e copie.
/// Stampante scelta: contesto di stampa, nome e pagine scelte (`None` = tutte).
type DialogChoice = (PrinterDc, String, Option<Vec<usize>>);

fn choose_in_dialog(files: &[PathBuf], p: &PrintArgs) -> Result<Option<DialogChoice>, String> {
    let (page_count, preset) = if files.len() == 1 {
        let (file, passwords, spec) = (files[0].clone(), p.passwords.clone(), p.pages.clone());
        std::thread::spawn(move || -> Result<(Option<usize>, Option<Vec<usize>>), String> {
            init_thread();
            let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let (doc, _) = open_with_passwords(&bytes, &passwords).map_err(|e| format!("{}: {e}", file.display()))?;
            let n = doc.page_count();
            let preset = spec.map(|s| parse_page_ranges(&s, n)).transpose()?;
            Ok((Some(n), preset))
        })
        .join()
        .map_err(|_| "errore interno nell'apertura del PDF".to_string())??
    } else {
        (None, None)
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let owner = dialog_owner();
    let mut settings = PrinterSettings::default();
    let opts = DialogOptions { page_count, current_page: None, preset_pages: preset, copies: p.copies };
    let result = print_dialog(owner, &mut settings, &opts);
    unsafe {
        let _ = DestroyWindow(owner);
    }
    Ok(result?.map(|c| (c.dc, settings.printer_name().unwrap_or_default(), c.pages)))
}

/// Finestra invisibile a cui appoggiare la finestra di stampa (che ne vuole una), al centro
/// dello schermo.
fn dialog_owner() -> HWND {
    unsafe {
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let (x, y) = ((work.left + work.right) / 2, (work.top + work.bottom) / 3);
        CreateWindowExW(WS_EX_TOOLWINDOW, w!("STATIC"), w!("myPDF"), WS_POPUP, x, y, 0, 0, None, None, None, None).unwrap_or_default()
    }
}

/// Stampa un file; restituisce le pagine mandate alla stampante (copie comprese).
/// `chosen` sono le pagine scelte nella finestra di stampa, che valgono più di --pages.
fn print_one(dc: &PrinterDc, file: &Path, p: &PrintArgs, chosen: Option<&[usize]>, copies: u32) -> Result<usize, String> {
    let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
    let (doc, password) = open_with_passwords(&bytes, &p.passwords)?;
    let page_count = doc.page_count();
    drop(doc);
    let one_copy: Vec<usize> = match (chosen, &p.pages) {
        (Some(c), _) => c.iter().copied().filter(|&i| i < page_count).collect(),
        (None, Some(spec)) => parse_page_ranges(spec, page_count)?,
        (None, None) => (0..page_count).collect(),
    };
    if one_copy.is_empty() {
        return Err("nessuna pagina da stampare".into());
    }
    // copie fascicolate: 1,2,3, 1,2,3, …
    let pages: Vec<usize> = (0..copies.max(1)).flat_map(|_| one_copy.iter().copied()).collect();
    let title = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    print_document(dc, &bytes, password.as_deref(), &title, &pages, p.output.as_deref(), &mut |_, _| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn senza_argomenti_apre_l_interfaccia() {
        assert_eq!(parse(&[]).unwrap(), Command::Gui(vec![]));
        assert_eq!(parse(&s(&["a.pdf", "b.pdf"])).unwrap(), Command::Gui(vec!["a.pdf".into(), "b.pdf".into()]));
        assert!(parse(&s(&["--boh"])).is_err());
    }

    #[test]
    fn unione_primo_nome_e_uscita() {
        let Command::Merge(m) = parse(&s(&["--merge", "out.pdf", "a.pdf", "b.pdf"])).unwrap() else { panic!() };
        assert_eq!(m.output, PathBuf::from("out.pdf"));
        assert_eq!(m.inputs, s(&["a.pdf", "b.pdf"]));
        assert!(!m.overwrite && m.bookmarks && !m.quiet);
    }

    #[test]
    fn unione_con_opzioni() {
        let Command::Merge(m) = parse(&s(&["/MERGE", "a.pdf", "-o", "unito", "b.pdf", "-y", "--no-bookmarks", "--password", "x", "-q"])).unwrap() else {
            panic!()
        };
        assert_eq!(m.output, PathBuf::from("unito.pdf"));
        assert_eq!(m.inputs, s(&["a.pdf", "b.pdf"]));
        assert!(m.overwrite && !m.bookmarks && m.quiet);
        assert_eq!(m.passwords, s(&["x"]));
        assert!(parse(&s(&["-m", "out.pdf"])).is_err());
        assert!(parse(&s(&["-m"])).is_err());
        assert!(parse(&s(&["-m", "o.pdf", "a.pdf", "--boh"])).is_err());
        assert!(parse(&s(&["-m", "-o"])).is_err());
        // dopo "--" tutto è un file, anche se inizia con "-"
        let Command::Merge(m) = parse(&s(&["-m", "o.pdf", "--", "-strano.pdf"])).unwrap() else { panic!() };
        assert_eq!(m.inputs, s(&["-strano.pdf"]));
    }

    #[test]
    fn stampa() {
        // senza stampante: finestra di stampa di Windows (anche per il verbo "Stampa" di Esplora file)
        let Command::Print(p) = parse(&s(&["/p", "doc.pdf"])).unwrap() else { panic!() };
        assert_eq!(p, PrintArgs::one("doc.pdf", None));
        assert!(p.uses_dialog());
        // "Stampa su" con la stampante: niente finestra
        let Command::Print(p) = parse(&s(&["/pt", "doc.pdf", "HP", "drv", "port"])).unwrap() else { panic!() };
        assert_eq!((p.files.clone(), p.printer.as_deref()), (s(&["doc.pdf"]), Some("HP")));
        assert!(!p.uses_dialog());
        let Command::Print(p) = parse(&s(&["--print", "a.pdf", "--printer", "X", "b.pdf", "--pages", "1-2", "--copies", "3", "-q"])).unwrap() else {
            panic!()
        };
        assert_eq!(p.files, s(&["a.pdf", "b.pdf"]));
        assert_eq!((p.printer.as_deref(), p.pages.as_deref(), p.copies, p.quiet), (Some("X"), Some("1-2"), 3, true));
        let Command::Print(p) = parse(&s(&["--stampa", "a.pdf", "--default-printer"])).unwrap() else { panic!() };
        assert!(p.default_printer && !p.uses_dialog());
        assert!(parse(&s(&["--print"])).is_err());
        assert!(parse(&s(&["--print", "a.pdf", "--copies", "0"])).is_err());
        assert!(parse(&s(&["--print", "a.pdf", "--copies", "tre"])).is_err());
        assert!(parse(&s(&["--print", "a.pdf", "--boh"])).is_err());
        assert_eq!(parse(&s(&["--printers"])).unwrap(), Command::Printers);
    }

    #[test]
    fn altri_comandi() {
        assert_eq!(parse(&s(&["/?"])).unwrap(), Command::Help);
        assert_eq!(parse(&s(&["--version"])).unwrap(), Command::Version);
        assert_eq!(parse(&s(&["--register"])).unwrap(), Command::Register);
        assert_eq!(parse(&s(&["--unregister"])).unwrap(), Command::Unregister);
    }
}
