//! myPDF: visualizzatore PDF minimale per Windows, con stampa e unione di più PDF in uno.
//!
//! - [`render`]: disegno delle pagine con il motore PDF integrato in Windows 10/11
//!   (`Windows.Data.Pdf`), quindi nessuna DLL esterna;
//! - [`merge`]: unione di più PDF (lopdf, tutto Rust);
//! - [`print`]: stampa con GDI su qualsiasi stampante;
//! - [`assoc`]: registrazione come programma per aprire i `.pdf`;
//! - [`cli`]: riga di comando (unione senza interfaccia, stampa, registrazione);
//! - [`gui`]: interfaccia Win32 nativa.

pub mod assoc;
pub mod cli;
pub mod files;
pub mod gui;
pub mod merge;
pub mod print;
pub mod render;
pub mod repair;
pub mod wide;

/// Versione mostrata in "Informazioni" e da `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Nome del programma (finestre, registro, segnalibri).
pub const APP_NAME: &str = "myPDF";
