//! Registrazione di myPDF come programma per aprire i file `.pdf` (solo per l'utente
//! corrente, senza diritti di amministratore).
//!
//! Windows 10 e 11 non permettono a un programma di rendersi predefinito da solo: lo si
//! registra (così compare in "Apri con" e nelle "App predefinite") e poi l'utente lo sceglie.

use crate::wide::WStr;
use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{ShellExecuteW, SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const PROG_ID: &str = "myPDF.Document";
const APP_KEY: &str = "myPDF";

fn check(e: WIN32_ERROR, what: &str) -> Result<(), String> {
    if e == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what}: errore {}", e.0))
    }
}

/// Scrive un valore stringa (`name` = `None` per il valore predefinito della chiave).
fn set_string(root: HKEY, path: &str, name: Option<&str>, value: &str) -> Result<(), String> {
    let p = WStr::new(path);
    let mut key = HKEY::default();
    unsafe {
        check(
            RegCreateKeyExW(root, p.pcwstr(), 0, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None),
            &format!("creazione di {path}"),
        )?;
        let n = name.map(WStr::new);
        let data: Vec<u8> = value.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
        let r = RegSetValueExW(key, n.as_ref().map(|n| n.pcwstr()).unwrap_or(PCWSTR::null()), 0, REG_SZ, Some(&data));
        let _ = RegCloseKey(key);
        check(r, &format!("scrittura in {path}"))
    }
}

/// Legge un valore stringa, se c'è.
pub fn get_string(root: HKEY, path: &str, name: Option<&str>) -> Option<String> {
    let p = WStr::new(path);
    let n = name.map(WStr::new);
    let np = n.as_ref().map(|n| n.pcwstr()).unwrap_or(PCWSTR::null());
    unsafe {
        let mut size = 0u32;
        if RegGetValueW(root, p.pcwstr(), np, RRF_RT_REG_SZ, None, None, Some(&mut size)) != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut size = (buf.len() * 2) as u32;
        if RegGetValueW(root, p.pcwstr(), np, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size)) != ERROR_SUCCESS {
            return None;
        }
        Some(crate::wide::from_wide(&buf))
    }
}

fn delete_tree(root: HKEY, path: &str) -> Result<(), String> {
    let p = WStr::new(path);
    let r = unsafe { RegDeleteTreeW(root, p.pcwstr()) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("eliminazione di {path}: errore {}", r.0))
    }
}

fn delete_value(root: HKEY, path: &str, name: &str) -> Result<(), String> {
    let p = WStr::new(path);
    let n = WStr::new(name);
    let r = unsafe { RegDeleteKeyValueW(root, p.pcwstr(), n.pcwstr()) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("eliminazione di {path}\\{name}: errore {}", r.0))
    }
}

fn exe_name(exe: &Path) -> String {
    exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "myPDF.exe".into())
}

/// Comando di apertura registrato per `exe`.
pub fn open_command(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

/// Registra `exe` per i PDF dell'utente corrente.
pub fn register(exe: &Path) -> Result<(), String> {
    register_in(HKEY_CURRENT_USER, "Software", exe)?;
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    Ok(())
}

/// Toglie la registrazione dell'utente corrente.
pub fn unregister(exe: &Path) -> Result<(), String> {
    unregister_in(HKEY_CURRENT_USER, "Software", exe)?;
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    Ok(())
}

/// Vero se i PDF sono registrati per proprio questo `exe`.
pub fn is_registered(exe: &Path) -> bool {
    get_string(HKEY_CURRENT_USER, &format!("Software\\Classes\\{PROG_ID}\\shell\\open\\command"), None).is_some_and(|c| c == open_command(exe))
}

/// Registrazione sotto `root\base` (`base` = "Software" per quella vera; i test usano una
/// chiave temporanea).
pub fn register_in(root: HKEY, base: &str, exe: &Path) -> Result<(), String> {
    let classes = format!("{base}\\Classes");
    let exe_s = exe.display().to_string();
    let open = open_command(exe);
    let prog = format!("{classes}\\{PROG_ID}");
    set_string(root, &prog, None, "Documento PDF")?;
    set_string(root, &prog, Some("FriendlyTypeName"), "Documento PDF")?;
    set_string(root, &format!("{prog}\\DefaultIcon"), None, &format!("{exe_s},0"))?;
    set_string(root, &format!("{prog}\\shell"), None, "open")?;
    set_string(root, &format!("{prog}\\shell\\open\\command"), None, &open)?;
    set_string(root, &format!("{prog}\\shell\\print\\command"), None, &format!("\"{exe_s}\" /p \"%1\""))?;
    set_string(root, &format!("{prog}\\shell\\printto\\command"), None, &format!("\"{exe_s}\" /pt \"%1\" \"%2\" \"%3\" \"%4\""))?;
    set_string(root, &format!("{classes}\\.pdf\\OpenWithProgids"), Some(PROG_ID), "")?;

    let app = format!("{classes}\\Applications\\{}", exe_name(exe));
    set_string(root, &app, Some("FriendlyAppName"), crate::APP_NAME)?;
    set_string(root, &format!("{app}\\DefaultIcon"), None, &format!("{exe_s},0"))?;
    set_string(root, &format!("{app}\\shell\\open\\command"), None, &open)?;
    set_string(root, &format!("{app}\\SupportedTypes"), Some(".pdf"), "")?;

    let caps = format!("{base}\\{APP_KEY}\\Capabilities");
    set_string(root, &caps, Some("ApplicationName"), crate::APP_NAME)?;
    set_string(root, &caps, Some("ApplicationDescription"), "Visualizzatore PDF minimale: anteprima, zoom, stampa e unione di PDF")?;
    set_string(root, &caps, Some("ApplicationIcon"), &format!("{exe_s},0"))?;
    set_string(root, &format!("{caps}\\FileAssociations"), Some(".pdf"), PROG_ID)?;
    set_string(root, &format!("{base}\\RegisteredApplications"), Some(APP_KEY), &caps)?;
    Ok(())
}

pub fn unregister_in(root: HKEY, base: &str, exe: &Path) -> Result<(), String> {
    let classes = format!("{base}\\Classes");
    delete_tree(root, &format!("{classes}\\{PROG_ID}"))?;
    delete_value(root, &format!("{classes}\\.pdf\\OpenWithProgids"), PROG_ID)?;
    delete_tree(root, &format!("{classes}\\Applications\\{}", exe_name(exe)))?;
    delete_tree(root, &format!("{base}\\{APP_KEY}\\Capabilities"))?;
    delete_value(root, &format!("{base}\\RegisteredApplications"), APP_KEY)?;
    Ok(())
}

/// Apre le "App predefinite" delle Impostazioni (su Windows 11 direttamente sulla pagina di myPDF).
pub fn open_default_apps_settings() {
    let uri = WStr::new(&format!("ms-settings:defaultapps?registeredAppUser={APP_KEY}"));
    let verb = WStr::new("open");
    unsafe {
        ShellExecuteW(None, verb.pcwstr(), uri.pcwstr(), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrazione_in_chiave_di_prova() {
        // Tutto sotto una chiave temporanea: le associazioni vere dell'utente non vengono toccate.
        let base = format!("Software\\myPDF-test-{}", std::process::id());
        let exe = Path::new(r"C:\Programmi prova\myPDF\myPDF.exe");
        register_in(HKEY_CURRENT_USER, &base, exe).unwrap();
        let cmd = get_string(HKEY_CURRENT_USER, &format!("{base}\\Classes\\{PROG_ID}\\shell\\open\\command"), None).unwrap();
        assert_eq!(cmd, r#""C:\Programmi prova\myPDF\myPDF.exe" "%1""#);
        assert_eq!(
            get_string(HKEY_CURRENT_USER, &format!("{base}\\{APP_KEY}\\Capabilities\\FileAssociations"), Some(".pdf")).as_deref(),
            Some(PROG_ID)
        );
        assert_eq!(get_string(HKEY_CURRENT_USER, &format!("{base}\\Classes\\.pdf\\OpenWithProgids"), Some(PROG_ID)).as_deref(), Some(""));
        assert!(get_string(HKEY_CURRENT_USER, &format!("{base}\\Classes\\Applications\\myPDF.exe\\shell\\open\\command"), None).is_some());
        unregister_in(HKEY_CURRENT_USER, &base, exe).unwrap();
        assert!(get_string(HKEY_CURRENT_USER, &format!("{base}\\Classes\\{PROG_ID}\\shell\\open\\command"), None).is_none());
        assert!(get_string(HKEY_CURRENT_USER, &format!("{base}\\RegisteredApplications"), Some(APP_KEY)).is_none());
        // Una seconda rimozione non è un errore.
        unregister_in(HKEY_CURRENT_USER, &base, exe).unwrap();
        delete_tree(HKEY_CURRENT_USER, &base).unwrap();
    }
}
