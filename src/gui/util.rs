//! Piccole utilità Win32 per l'interfaccia: finestre di dialogo dei file, messaggi, font.

use crate::wide::{path_from_wide, WStr};
use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject, HFONT};
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOREADONLYRETURN,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, SendMessageW, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO, IDYES, NONCLIENTMETRICSW,
    SPI_GETNONCLIENTMETRICS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_SETFONT,
};

const PDF_FILTER: &str = "Documenti PDF (*.pdf)\0*.pdf\0Tutti i file (*.*)\0*.*\0\0";

/// Scegli uno o più PDF da aprire.
pub fn open_files_dialog(owner: HWND, initial_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut buf = vec![0u16; 65536];
    let filter: Vec<u16> = PDF_FILTER.encode_utf16().collect();
    let dir = initial_dir.map(|d| WStr::from_os(d.as_os_str()));
    let title = WStr::new("Apri PDF");
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        nFilterIndex: 1,
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrInitialDir: dir.as_ref().map(|d| d.pcwstr()).unwrap_or(PCWSTR::null()),
        lpstrTitle: title.pcwstr(),
        Flags: OFN_ALLOWMULTISELECT | OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };
    if !unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
        return Vec::new();
    }
    parse_multi_select(&buf)
}

/// Risultato di una scelta multipla: "cartella\0file1\0file2\0\0" oppure "percorso\0\0".
fn parse_multi_select(buf: &[u16]) -> Vec<PathBuf> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (i, &c) in buf.iter().enumerate() {
        if c == 0 {
            if i == start {
                break;
            }
            parts.push(&buf[start..i]);
            start = i + 1;
        }
    }
    match parts.len() {
        0 => Vec::new(),
        1 => vec![path_from_wide(parts[0])],
        _ => {
            let dir = path_from_wide(parts[0]);
            parts[1..].iter().map(|f| dir.join(path_from_wide(f))).collect()
        }
    }
}

/// Scegli dove salvare un PDF.
pub fn save_file_dialog(owner: HWND, initial: &Path, title: &str) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32768];
    let name: Vec<u16> = initial.file_name().map(|n| n.to_string_lossy().encode_utf16().collect()).unwrap_or_default();
    buf[..name.len().min(32000)].copy_from_slice(&name[..name.len().min(32000)]);
    let dir = initial.parent().map(|d| WStr::from_os(d.as_os_str()));
    let filter: Vec<u16> = "Documento PDF (*.pdf)\0*.pdf\0\0".encode_utf16().collect();
    let title = WStr::new(title);
    let ext = WStr::new("pdf");
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        nFilterIndex: 1,
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrInitialDir: dir.as_ref().map(|d| d.pcwstr()).unwrap_or(PCWSTR::null()),
        lpstrTitle: title.pcwstr(),
        lpstrDefExt: ext.pcwstr(),
        Flags: OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOREADONLYRETURN | OFN_HIDEREADONLY,
        ..Default::default()
    };
    if !unsafe { GetSaveFileNameW(&mut ofn) }.as_bool() {
        return None;
    }
    Some(path_from_wide(&buf))
}

pub fn message(owner: HWND, text: &str, kind: MsgKind) {
    let t = WStr::new(text);
    let c = WStr::new(crate::APP_NAME);
    let icon = match kind {
        MsgKind::Info => MB_ICONINFORMATION,
        MsgKind::Error => MB_ICONERROR,
    };
    unsafe {
        MessageBoxW(owner, t.pcwstr(), c.pcwstr(), MB_OK | icon);
    }
}

pub fn ask_yes_no(owner: HWND, text: &str) -> bool {
    let t = WStr::new(text);
    let c = WStr::new(crate::APP_NAME);
    unsafe { MessageBoxW(owner, t.pcwstr(), c.pcwstr(), MB_YESNO | MB_ICONQUESTION) == IDYES }
}

#[derive(Clone, Copy)]
pub enum MsgKind {
    Info,
    Error,
}

/// Font dei messaggi di Windows alla risoluzione `dpi`.
pub fn message_font(dpi: u32) -> HFONT {
    let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    unsafe {
        let ok = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0).0,
            dpi,
        );
        if ok.is_err() {
            return HFONT::default();
        }
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

pub fn set_font(hwnd: HWND, font: HFONT) {
    unsafe {
        SendMessageW(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

/// Larghezza in pixel di `text` scritto con `font`.
pub fn text_width(font: HFONT, text: &str) -> i32 {
    let w: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, font);
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &w, &mut size);
        SelectObject(dc, old);
        ReleaseDC(None, dc);
        size.cx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scelta_multipla() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let mut one = w("C:\\a\\b.pdf");
        one.extend([0, 0]);
        assert_eq!(parse_multi_select(&one), vec![PathBuf::from("C:\\a\\b.pdf")]);
        let mut many = w("C:\\dir");
        many.push(0);
        many.extend(w("x.pdf"));
        many.push(0);
        many.extend(w("y.pdf"));
        many.extend([0, 0]);
        assert_eq!(parse_multi_select(&many), vec![PathBuf::from("C:\\dir\\x.pdf"), PathBuf::from("C:\\dir\\y.pdf")]);
        assert!(parse_multi_select(&[0, 0]).is_empty());
    }
}
