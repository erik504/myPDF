//! Finestrella modale per chiedere la password di un PDF protetto.

use super::util::{message_font, set_font};
use crate::wide::{from_wide, WStr};
use std::cell::{Cell, RefCell};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, GetSysColorBrush, COLOR_BTNFACE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

thread_local! {
    static RESULT: RefCell<Option<String>> = const { RefCell::new(None) };
    static DONE: Cell<bool> = const { Cell::new(false) };
}

const ID_EDIT: i32 = 10;
const IDOK_: i32 = 1;
const IDCANCEL_: i32 = 2;

/// Chiede la password per `file_name`. `None` se l'utente annulla.
pub fn ask_password(owner: HWND, file_name: &str, wrong: bool) -> Option<String> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let class = w!("myPDF.Prompt");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc_),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc); // la seconda volta fallisce: va bene

        let dpi = GetDpiForWindow(owner).max(96);
        let px = |v: i32| v * dpi as i32 / 96;
        let (w, h) = (px(400), px(170));
        let mut orc = RECT::default();
        let _ = GetWindowRect(owner, &mut orc);
        let x = orc.left + ((orc.right - orc.left) - w) / 2;
        let y = orc.top + ((orc.bottom - orc.top) - h) / 3;
        let title = WStr::new("Password richiesta");
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            class,
            title.pcwstr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            owner,
            None,
            hinst,
            None,
        )
        .ok()?;
        let font = message_font(dpi);
        let mut crc = RECT::default();
        let _ = GetClientRect(dlg, &mut crc);
        let cw = crc.right;
        let text = if wrong {
            format!("La password non è corretta.\nRiprova per \"{file_name}\":")
        } else {
            format!("\"{file_name}\" è protetto da password.\nInserisci la password per aprirlo:")
        };
        let t = WStr::new(&text);
        let lbl = CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), t.pcwstr(), WS_CHILD | WS_VISIBLE, px(12), px(10), cw - px(24), px(40), dlg, None, hinst, None).ok()?;
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE((ES_PASSWORD | ES_AUTOHSCROLL) as u32),
            px(12),
            px(54),
            cw - px(24),
            px(24),
            dlg,
            HMENU(ID_EDIT as _),
            hinst,
            None,
        )
        .ok()?;
        let ok = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("OK"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            cw - px(184),
            crc.bottom - px(40),
            px(84),
            px(28),
            dlg,
            HMENU(IDOK_ as _),
            hinst,
            None,
        )
        .ok()?;
        let cancel = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Annulla"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            cw - px(94),
            crc.bottom - px(40),
            px(84),
            px(28),
            dlg,
            HMENU(IDCANCEL_ as _),
            hinst,
            None,
        )
        .ok()?;
        for c in [lbl, edit, ok, cancel] {
            set_font(c, font);
        }

        RESULT.with(|r| *r.borrow_mut() = None);
        DONE.with(|d| d.set(false));
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetFocus(edit);
        let mut msg = MSG::default();
        let mut quit = None;
        while !DONE.with(|d| d.get()) {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 {
                quit = Some(msg.wParam.0 as i32);
                break;
            }
            if r.0 == -1 {
                break;
            }
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // Riabilitare la finestra principale prima di chiudere questa, così torna in primo piano.
        let _ = EnableWindow(owner, true);
        if IsWindow(dlg).as_bool() {
            let _ = DestroyWindow(dlg);
        }
        let _ = SetForegroundWindow(owner);
        let _ = DeleteObject(font);
        if let Some(code) = quit {
            PostQuitMessage(code);
        }
        RESULT.with(|r| r.borrow_mut().take())
    }
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            if id == IDOK_ {
                let edit = GetDlgItem(hwnd, ID_EDIT).unwrap_or_default();
                let len = GetWindowTextLengthW(edit);
                let mut buf = vec![0u16; len as usize + 1];
                GetWindowTextW(edit, &mut buf);
                RESULT.with(|r| *r.borrow_mut() = Some(from_wide(&buf)));
                DONE.with(|d| d.set(true));
            } else if id == IDCANCEL_ {
                DONE.with(|d| d.set(true));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            DONE.with(|d| d.set(true));
            LRESULT(0)
        }
        // Per IsDialogMessage: Invio preme OK.
        DM_GETDEFID => LRESULT(((DC_HASDEFID as isize) << 16) | IDOK_ as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
