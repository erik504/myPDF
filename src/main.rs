// Programma a finestre: aprendo un PDF da Esplora file non compare nessuna console.
// I comandi da riga di comando si agganciano alla console di chi li lancia (vedi cli.rs).
#![windows_subsystem = "windows"]

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let code = mypdf::cli::main(args);
    std::process::exit(code);
}
