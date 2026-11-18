//! Interfaccia Win32 nativa: elenco dei documenti (l'ordine di unione) a sinistra,
//! visualizzatore a destra, pulsanti in alto e barra di stato in basso.
//!
//! Lo stato vive in un'unica struttura [`App`] per thread (dentro un `RefCell`). Le funzioni di
//! Windows che inviano messaggi in modo sincrono possono rientrare nelle procedure di finestra
//! mentre lo stato è in uso: in quel caso il lavoro viene rimandato con un messaggio. Le
//! finestre modali (messaggi, scelta file, stampa) si aprono sempre a stato rilasciato.

mod prompt;
mod util;
mod viewer;
mod worker;

use crate::assoc;
use crate::files::{is_pdf, natural_cmp, sort_paths};
use crate::merge::{merge_files, MergeError, MergeOptions, MergeReport};
use crate::print::{print_dialog, print_document, DialogOptions, PrinterSettings};
use crate::render::LoadError;
use crate::wide::{from_wide, path_from_wide, WStr};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use util::{ask_yes_no, message, message_font, set_font, text_width, MsgKind};
use viewer::{SavedView, Viewer, ZoomMode};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, DeleteObject, EndPaint, GetSysColorBrush, ScreenToClient, COLOR_BTNFACE, HFONT, PAINTSTRUCT};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, SS_CENTER, SS_CENTERIMAGE, SS_ENDELLIPSIS, SS_LEFT};
use windows::Win32::UI::Controls::{
    DrawInsert, InitCommonControlsEx, LBItemFromPt, MakeDragList, DL_BEGINDRAG, DL_CANCELDRAG, DL_DRAGGING, DL_DROPPED, DL_MOVECURSOR, DL_STOPCURSOR,
    DRAGLISTINFO, DRAGLISTMSGSTRING, EM_SETSEL, ICC_BAR_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, SBARS_SIZEGRIP, SB_SETPARTS, SB_SETTEXTW,
    STATUSCLASSNAMEW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, GetKeyState, IsWindowEnabled, ReleaseCapture, SetCapture, SetFocus, VIRTUAL_KEY, VK_ADD, VK_CONTROL, VK_DOWN, VK_END,
    VK_ESCAPE, VK_F9, VK_HOME, VK_LEFT, VK_NEXT, VK_NUMPAD0, VK_OEM_MINUS, VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE,
    VK_SUBTRACT, VK_UP,
};
use windows::Win32::UI::Shell::{DefSubclassProc, DragAcceptFiles, DragFinish, DragQueryFileW, SetWindowSubclass, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_WORKER: u32 = WM_APP + 1;
const WM_APP_ASK_PASSWORD: u32 = WM_APP + 2;
const WM_APP_MERGE_DONE: u32 = WM_APP + 3;
const WM_APP_PRINT_PROGRESS: u32 = WM_APP + 4;
const WM_APP_PRINT_DONE: u32 = WM_APP + 5;
const WM_APP_RELAYOUT: u32 = WM_APP + 6;
const WM_APP_VIEW_LAYOUT: u32 = WM_APP + 7;

// Comandi (menu, scorciatoie e pulsanti usano gli stessi numeri).
const IDM_OPEN: u16 = 1001;
const IDM_MERGE: u16 = 1002;
const IDM_PRINT: u16 = 1003;
const IDM_CLOSE_DOC: u16 = 1004;
const IDM_CLOSE_ALL: u16 = 1005;
const IDM_EXIT: u16 = 1006;
const IDM_ZOOM_IN: u16 = 1010;
const IDM_ZOOM_OUT: u16 = 1011;
const IDM_ZOOM_100: u16 = 1012;
const IDM_FIT_WIDTH: u16 = 1013;
const IDM_FIT_PAGE: u16 = 1014;
const IDM_PREV: u16 = 1015;
const IDM_NEXT: u16 = 1016;
const IDM_GOTO: u16 = 1017;
const IDM_TOGGLE_PANEL: u16 = 1018;
const IDM_FIRST: u16 = 1019;
const IDM_LAST: u16 = 1020;
const IDM_MOVE_UP: u16 = 1030;
const IDM_MOVE_DOWN: u16 = 1031;
const IDM_SORT: u16 = 1032;
const IDM_ASSOC: u16 = 1040;
const IDM_UNASSOC: u16 = 1041;
const IDM_CLI_HELP: u16 = 1042;
const IDM_ABOUT: u16 = 1043;
const IDM_GOTO_FROM_EDIT: u16 = 1050;

const ID_LIST: u16 = 2001;
const ID_EDIT_PAGE: u16 = 2002;
const ID_STATIC: u16 = 2003;
const ID_STATUS: u16 = 2004;
const ID_VIEWER: u16 = 2005;

/// Spessore del divisore tra elenco e pagine (in pixel a 96 dpi).
const SPLITTER: i32 = 6;

enum DocState {
    Loading,
    /// Pagine (DIP) e se il file era danneggiato (solo le pagine recuperate).
    Ready(Vec<(f32, f32)>, bool),
    NeedsPassword {
        wrong: bool,
    },
    /// Il file non è un PDF.
    NotPdf,
    /// PDF così danneggiato che nessuna pagina è leggibile.
    Broken(String),
    Error(String),
}

struct Doc {
    id: u64,
    path: PathBuf,
    name: String,
    password: Option<String>,
    state: DocState,
    view: Option<SavedView>,
}

#[derive(Default)]
struct Controls {
    btn_open: HWND,
    btn_merge: HWND,
    btn_print: HWND,
    btn_zoom_out: HWND,
    lbl_zoom: HWND,
    btn_zoom_in: HWND,
    btn_fit_w: HWND,
    btn_fit_p: HWND,
    btn_prev: HWND,
    edit_page: HWND,
    lbl_pages: HWND,
    btn_next: HWND,
    lbl_list: HWND,
    list: HWND,
    btn_up: HWND,
    btn_down: HWND,
    btn_remove: HWND,
    btn_clear: HWND,
    btn_merge2: HWND,
    status: HWND,
}

struct App {
    hwnd: HWND,
    c: Controls,
    font: HFONT,
    dpi: u32,
    panel_w96: i32,
    panel_visible: bool,
    docs: Vec<Doc>,
    current: Option<usize>,
    next_id: u64,
    viewer: Viewer,
    worker: worker::Worker,
    printer: PrinterSettings,
    merging: bool,
    printing: bool,
    status_msg: Option<String>,
    asking_password: bool,
    splitter: Option<i32>,
    drag_from: Option<usize>,
    last_dir: Option<PathBuf>,
    shown: Option<(usize, usize, i32)>,
    shown_status: [String; 3],
    shown_title: String,
    merge_rx: Option<Receiver<Result<(MergeReport, PathBuf), MergeError>>>,
    print_rx: Option<Receiver<Result<usize, String>>>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static MAIN: Cell<isize> = const { Cell::new(0) };
    static VIEWER: Cell<isize> = const { Cell::new(0) };
    static LAST_FOCUS: Cell<isize> = const { Cell::new(0) };
    static DRAG_MSG: Cell<u32> = const { Cell::new(0) };
}

fn hwnd_of(v: isize) -> HWND {
    HWND(v as *mut _)
}

/// Esegue `f` sullo stato dell'applicazione, se non è già in uso (rientro) e se esiste.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        let app = guard.as_mut()?;
        Some(f(app))
    })
}

fn post(hwnd: HWND, msg: u32, w: usize, l: isize) {
    unsafe {
        let _ = PostMessageW(hwnd, msg, WPARAM(w), LPARAM(l));
    }
}

fn set_text(hwnd: HWND, text: &str) {
    let t = WStr::new(text);
    unsafe {
        let _ = SetWindowTextW(hwnd, t.pcwstr());
    }
}

fn get_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        GetWindowTextW(hwnd, &mut buf);
        from_wide(&buf)
    }
}

fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}
fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}
fn point_from_lparam(l: LPARAM) -> (i32, i32) {
    ((l.0 & 0xFFFF) as i16 as i32, ((l.0 >> 16) & 0xFFFF) as i16 as i32)
}

const HINT: &str = "Trascina qui uno o più file PDF, oppure usa «Apri…» (Ctrl+O).\n\n\
I documenti aperti compaiono nell'elenco a sinistra: riordinali trascinandoli o con «Su» e «Giù», \
poi premi «Unisci in un PDF…» per crearne uno solo, nello stesso ordine.";

/// Avvia l'interfaccia aprendo `files`. Restituisce il codice di uscita.
pub fn run(files: Vec<PathBuf>) -> i32 {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES };
        let _ = InitCommonControlsEx(&icc);
        DRAG_MSG.with(|m| m.set(RegisterWindowMessageW(DRAGLISTMSGSTRING)));

        let Ok(hmod) = GetModuleHandleW(None) else { return 1 };
        let hinst = hmod.into();
        let icon = LoadIconW(hinst, PCWSTR(1 as _)).unwrap_or_default();
        let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let main_class = w!("myPDF.Main");
        let viewer_class = w!("myPDF.Viewer");
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(main_proc),
            hInstance: hinst,
            hIcon: icon,
            hIconSm: icon,
            hCursor: arrow,
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszClassName: main_class,
            ..Default::default()
        });
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(viewer_proc),
            hInstance: hinst,
            hCursor: arrow,
            lpszClassName: viewer_class,
            ..Default::default()
        });

        let menu = build_menu();
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            main_class,
            w!("myPDF"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1200,
            800,
            None,
            menu,
            hinst,
            None,
        ) else {
            return 1;
        };
        MAIN.with(|m| m.set(hwnd.0 as isize));
        let dpi = GetDpiForWindow(hwnd).max(96);
        place_initial(hwnd, dpi);

        let child = |class: PCWSTR, text: &str, style: u32, ex: WINDOW_EX_STYLE, id: u16| -> HWND {
            let t = WStr::new(text);
            CreateWindowExW(ex, class, t.pcwstr(), WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style), 0, 0, 10, 10, hwnd, HMENU(id as usize as *mut _), hinst, None)
                .unwrap_or_default()
        };
        let btn = |text: &str, id: u16| child(w!("BUTTON"), text, (BS_PUSHBUTTON as u32) | WS_TABSTOP.0, WINDOW_EX_STYLE(0), id);
        let label = |text: &str, style: u32| child(w!("STATIC"), text, style, WINDOW_EX_STYLE(0), ID_STATIC);
        let c = Controls {
            btn_open: btn("Apri…", IDM_OPEN),
            btn_merge: btn("Unisci…", IDM_MERGE),
            btn_print: btn("Stampa…", IDM_PRINT),
            btn_zoom_out: btn("−", IDM_ZOOM_OUT),
            lbl_zoom: label("100%", SS_CENTER.0 | SS_CENTERIMAGE.0),
            btn_zoom_in: btn("+", IDM_ZOOM_IN),
            btn_fit_w: btn("Larghezza", IDM_FIT_WIDTH),
            btn_fit_p: btn("Pagina intera", IDM_FIT_PAGE),
            btn_prev: btn("◀", IDM_PREV),
            edit_page: child(w!("EDIT"), "", (ES_CENTER | ES_NUMBER | ES_AUTOHSCROLL) as u32 | WS_TABSTOP.0, WS_EX_CLIENTEDGE, ID_EDIT_PAGE),
            lbl_pages: label("", SS_LEFT.0 | SS_CENTERIMAGE.0),
            btn_next: btn("▶", IDM_NEXT),
            lbl_list: label("Documenti (ordine di unione):", SS_LEFT.0 | SS_CENTERIMAGE.0 | SS_ENDELLIPSIS.0),
            list: child(
                w!("LISTBOX"),
                "",
                (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32 | WS_VSCROLL.0 | WS_TABSTOP.0,
                WS_EX_CLIENTEDGE,
                ID_LIST,
            ),
            btn_up: btn("↑ Su", IDM_MOVE_UP),
            btn_down: btn("↓ Giù", IDM_MOVE_DOWN),
            btn_remove: btn("Togli", IDM_CLOSE_DOC),
            btn_clear: btn("Svuota", IDM_CLOSE_ALL),
            btn_merge2: child(w!("BUTTON"), "Unisci in un PDF…", (BS_DEFPUSHBUTTON as u32) | WS_TABSTOP.0, WINDOW_EX_STYLE(0), IDM_MERGE),
            status: child(STATUSCLASSNAMEW, "", SBARS_SIZEGRIP, WINDOW_EX_STYLE(0), ID_STATUS),
        };
        let viewer_hwnd = child(viewer_class, "", WS_VSCROLL.0 | WS_HSCROLL.0 | WS_TABSTOP.0, WINDOW_EX_STYLE(0), ID_VIEWER);
        VIEWER.with(|v| v.set(viewer_hwnd.0 as isize));
        let _ = MakeDragList(c.list);
        let _ = SetWindowSubclass(c.edit_page, Some(edit_subclass), 1, 0);
        DragAcceptFiles(hwnd, true);
        DragAcceptFiles(viewer_hwnd, true);

        let worker = worker::Worker::start(hwnd, WM_APP_WORKER);
        let mut viewer = Viewer::new(viewer_hwnd, dpi);
        viewer.message = Some(HINT.into());
        let app = App {
            hwnd,
            c,
            font: HFONT::default(),
            dpi,
            panel_w96: 250,
            panel_visible: true,
            docs: Vec::new(),
            current: None,
            next_id: 1,
            viewer,
            worker,
            printer: PrinterSettings::default(),
            merging: false,
            printing: false,
            status_msg: None,
            asking_password: false,
            splitter: None,
            drag_from: None,
            last_dir: None,
            shown: None,
            shown_status: Default::default(),
            shown_title: String::new(),
            merge_rx: None,
            print_rx: None,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(|a| {
            a.apply_fonts();
            a.layout();
            a.viewer.resized();
            a.add_files(files, true);
            a.sync_ui();
        });

        let no_activate = std::env::var_os("MYPDF_NOACTIVATE").is_some();
        let _ = ShowWindow(hwnd, if no_activate { SW_SHOWNOACTIVATE } else { SW_SHOWNORMAL });
        if !no_activate {
            let _ = SetFocus(viewer_hwnd);
        }

        let accel = build_accelerators();
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 || r.0 == -1 {
                break;
            }
            let main = hwnd_of(MAIN.with(|m| m.get()));
            if !accel.is_invalid() && !main.0.is_null() && TranslateAcceleratorW(main, accel, &msg) != 0 {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !accel.is_invalid() {
            let _ = DestroyAcceleratorTable(accel);
        }
        msg.wParam.0 as i32
    }
}

/// Dimensioni iniziali: una buona parte dello schermo, al centro.
fn place_initial(hwnd: HWND, dpi: u32) {
    unsafe {
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let (ww, wh) = (work.right - work.left, work.bottom - work.top);
        let s = dpi as f64 / 96.0;
        let w = ((1280.0 * s) as i32).min(ww * 92 / 100).max(400);
        let h = ((920.0 * s) as i32).min(wh * 94 / 100).max(300);
        let _ = SetWindowPos(hwnd, None, work.left + (ww - w) / 2, work.top + (wh - h) / 2, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

fn build_menu() -> HMENU {
    unsafe {
        let bar = CreateMenu().unwrap_or_default();
        let add = |m: HMENU, id: u16, text: &str| {
            let t = WStr::new(text);
            let _ = AppendMenuW(m, MF_STRING, id as usize, t.pcwstr());
        };
        let sep = |m: HMENU| {
            let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
        };
        let popup = |title: &str| -> HMENU {
            let p = CreatePopupMenu().unwrap_or_default();
            let t = WStr::new(title);
            let _ = AppendMenuW(bar, MF_POPUP, p.0 as usize, t.pcwstr());
            p
        };
        let file = popup("&File");
        add(file, IDM_OPEN, "&Apri…\tCtrl+O");
        add(file, IDM_MERGE, "&Unisci in un PDF…\tCtrl+M");
        add(file, IDM_PRINT, "&Stampa…\tCtrl+P");
        sep(file);
        add(file, IDM_CLOSE_DOC, "&Chiudi documento\tCtrl+W");
        add(file, IDM_CLOSE_ALL, "Chiudi &tutti");
        sep(file);
        add(file, IDM_EXIT, "&Esci");
        let view = popup("&Visualizza");
        add(view, IDM_ZOOM_IN, "Zoom &avanti\tCtrl++");
        add(view, IDM_ZOOM_OUT, "Zoom &indietro\tCtrl+-");
        add(view, IDM_ZOOM_100, "&Dimensioni reali (100%)\tCtrl+1");
        add(view, IDM_FIT_WIDTH, "Adatta alla &larghezza\tCtrl+2");
        add(view, IDM_FIT_PAGE, "&Pagina intera\tCtrl+0");
        sep(view);
        add(view, IDM_FIRST, "Prima pagina\tHome");
        add(view, IDM_PREV, "Pagina p&recedente\tPagSu");
        add(view, IDM_NEXT, "Pagina &successiva\tPagGiù");
        add(view, IDM_LAST, "Ultima pagina\tFine");
        add(view, IDM_GOTO, "&Vai alla pagina…\tCtrl+G");
        sep(view);
        add(view, IDM_TOGGLE_PANEL, "Elenco dei &documenti\tF9");
        let docs = popup("&Documenti");
        add(docs, IDM_MOVE_UP, "Sposta &su\tAlt+Su");
        add(docs, IDM_MOVE_DOWN, "Sposta &giù\tAlt+Giù");
        add(docs, IDM_SORT, "&Ordina per nome");
        sep(docs);
        add(docs, IDM_CLOSE_DOC, "&Togli dall'elenco\tCtrl+W");
        add(docs, IDM_CLOSE_ALL, "S&vuota l'elenco");
        sep(docs);
        add(docs, IDM_MERGE, "&Unisci in un PDF…\tCtrl+M");
        let help = popup("&?");
        add(help, IDM_ASSOC, "Usa myPDF per aprire i &PDF…");
        add(help, IDM_UNASSOC, "&Non usare più myPDF per i PDF");
        sep(help);
        add(help, IDM_CLI_HELP, "Uso da &riga di comando");
        add(help, IDM_ABOUT, "&Informazioni su myPDF");
        bar
    }
}

fn build_accelerators() -> HACCEL {
    let v = |k: VIRTUAL_KEY| k.0;
    let c = FVIRTKEY | FCONTROL;
    let table: Vec<(ACCEL_VIRT_FLAGS, u16, u16)> = vec![
        (c, b'O' as u16, IDM_OPEN),
        (c, b'M' as u16, IDM_MERGE),
        (c, b'P' as u16, IDM_PRINT),
        (c, b'W' as u16, IDM_CLOSE_DOC),
        (c, v(VK_OEM_PLUS), IDM_ZOOM_IN),
        (c, v(VK_ADD), IDM_ZOOM_IN),
        (c, v(VK_OEM_MINUS), IDM_ZOOM_OUT),
        (c, v(VK_SUBTRACT), IDM_ZOOM_OUT),
        (c, b'0' as u16, IDM_FIT_PAGE),
        (c, v(VK_NUMPAD0), IDM_FIT_PAGE),
        (c, b'1' as u16, IDM_ZOOM_100),
        (c, b'2' as u16, IDM_FIT_WIDTH),
        (c, b'G' as u16, IDM_GOTO),
        (FVIRTKEY, v(VK_F9), IDM_TOGGLE_PANEL),
        (FVIRTKEY | FALT, v(VK_UP), IDM_MOVE_UP),
        (FVIRTKEY | FALT, v(VK_DOWN), IDM_MOVE_DOWN),
    ];
    let accels: Vec<ACCEL> = table.into_iter().map(|(f, key, cmd)| ACCEL { fVirt: f, key, cmd }).collect();
    unsafe { CreateAcceleratorTableW(&accels).unwrap_or_default() }
}

impl App {
    fn px(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
    }

    fn all_controls(&self) -> Vec<HWND> {
        let c = &self.c;
        vec![
            c.btn_open, c.btn_merge, c.btn_print, c.btn_zoom_out, c.lbl_zoom, c.btn_zoom_in, c.btn_fit_w, c.btn_fit_p, c.btn_prev, c.edit_page, c.lbl_pages,
            c.btn_next, c.lbl_list, c.list, c.btn_up, c.btn_down, c.btn_remove, c.btn_clear, c.btn_merge2, c.status,
        ]
    }

    fn apply_fonts(&mut self) {
        let old = self.font;
        self.font = message_font(self.dpi);
        for h in self.all_controls() {
            set_font(h, self.font);
        }
        self.viewer.font = self.font;
        if !old.is_invalid() {
            unsafe {
                let _ = DeleteObject(old);
            }
        }
    }

    fn panel_px(&self) -> i32 {
        if self.panel_visible {
            self.px(self.panel_w96)
        } else {
            0
        }
    }

    /// Posiziona tutti i controlli.
    fn layout(&mut self) {
        unsafe {
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let (w, h) = (rc.right, rc.bottom);
            SendMessageW(self.c.status, WM_SIZE, WPARAM(0), LPARAM(0));
            let mut src = RECT::default();
            let _ = GetWindowRect(self.c.status, &mut src);
            let status_h = src.bottom - src.top;
            let parts = [w - self.px(300), w - self.px(110), -1];
            SendMessageW(self.c.status, SB_SETPARTS, WPARAM(parts.len()), LPARAM(parts.as_ptr() as isize));

            let tb_h = self.px(42);
            let bh = self.px(28);
            let by = (tb_h - bh) / 2;
            let mut x = self.px(6);
            let gap = self.px(4);
            let sep = self.px(14);
            let place = |hwnd: HWND, x: i32, y: i32, cw: i32, ch: i32| {
                let _ = SetWindowPos(hwnd, None, x, y, cw, ch, SWP_NOZORDER | SWP_NOACTIVATE);
            };
            let bw = |text: &str, min: i32| (text_width(self.font, text) + self.px(22)).max(self.px(min));
            let c = &self.c;
            for (hwnd, text, min, sep_after) in [
                (c.btn_open, "Apri…", 70, true),
                (c.btn_merge, "Unisci…", 70, false),
                (c.btn_print, "Stampa…", 70, true),
                (c.btn_zoom_out, "−", 30, false),
            ] {
                let bwid = bw(text, min);
                place(hwnd, x, by, bwid, bh);
                x += bwid + if sep_after { sep } else { gap };
            }
            let zl = text_width(self.font, "1600%") + self.px(8);
            place(c.lbl_zoom, x, by, zl, bh);
            x += zl + gap;
            for (hwnd, text, min, sep_after) in [
                (c.btn_zoom_in, "+", 30, false),
                (c.btn_fit_w, "Larghezza", 60, false),
                (c.btn_fit_p, "Pagina intera", 60, true),
                (c.btn_prev, "◀", 30, false),
            ] {
                let bwid = bw(text, min);
                place(hwnd, x, by, bwid, bh);
                x += bwid + if sep_after { sep } else { gap };
            }
            let ew = text_width(self.font, "00000") + self.px(14);
            place(c.edit_page, x, by + self.px(2), ew, bh - self.px(4));
            x += ew + gap;
            let lw = text_width(self.font, "di 00000") + self.px(4);
            place(c.lbl_pages, x, by, lw, bh);
            x += lw + gap;
            place(c.btn_next, x, by, bw("▶", 30), bh);

            let top = tb_h;
            let bottom = h - status_h;
            let panel = self.panel_px();
            let panel_ctrls = [c.lbl_list, c.list, c.btn_up, c.btn_down, c.btn_remove, c.btn_clear, c.btn_merge2];
            for p in panel_ctrls {
                let _ = ShowWindow(p, if self.panel_visible { SW_SHOWNA } else { SW_HIDE });
            }
            if self.panel_visible {
                let m = self.px(6);
                let lh = self.px(22);
                place(c.lbl_list, m, top, panel - 2 * m, lh);
                let merge_h = self.px(34);
                let merge_y = bottom - m - merge_h;
                place(c.btn_merge2, m, merge_y, panel - 2 * m, merge_h);
                let row_y = merge_y - gap - bh;
                let n = 4;
                let rw = (panel - 2 * m - (n - 1) * gap) / n;
                for (i, b) in [c.btn_up, c.btn_down, c.btn_remove, c.btn_clear].into_iter().enumerate() {
                    place(b, m + i as i32 * (rw + gap), row_y, rw, bh);
                }
                let list_y = top + lh + self.px(2);
                place(c.list, m, list_y, panel - 2 * m, (row_y - gap - list_y).max(self.px(40)));
            }
            let vx = if self.panel_visible { panel + self.px(SPLITTER) - self.px(6) } else { 0 };
            place(self.viewer.hwnd, vx, top, (w - vx).max(1), (bottom - top).max(1));
        }
    }

    /// Aggiunge file all'elenco (le cartelle diventano i PDF che contengono).
    fn add_files(&mut self, paths: Vec<PathBuf>, select_first: bool) -> usize {
        let mut expanded = Vec::new();
        for p in paths {
            if p.is_dir() {
                let mut found: Vec<PathBuf> =
                    std::fs::read_dir(&p).map(|rd| rd.flatten().map(|e| e.path()).filter(|q| q.is_file() && is_pdf(q)).collect()).unwrap_or_default();
                sort_paths(&mut found);
                expanded.extend(found);
            } else {
                expanded.push(p);
            }
        }
        let first_new = self.docs.len();
        for path in expanded {
            let path = std::path::absolute(&path).unwrap_or(path);
            let id = self.next_id;
            self.next_id += 1;
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
            if let Some(dir) = path.parent() {
                self.last_dir = Some(dir.to_path_buf());
            }
            self.worker.open(id, path.clone(), None);
            self.docs.push(Doc { id, path, name, password: None, state: DocState::Loading, view: None });
        }
        let added = self.docs.len() - first_new;
        self.refresh_list();
        if added > 0 && (select_first || self.current.is_none()) {
            self.select(first_new);
        }
        added
    }

    fn doc_index(&self, id: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    fn list_label(i: usize, d: &Doc) -> String {
        let extra = match &d.state {
            DocState::Loading => "  (…)".to_string(),
            DocState::Ready(p, damaged) => {
                let n = if p.len() == 1 { "1 pagina".to_string() } else { format!("{} pagine", p.len()) };
                if *damaged {
                    format!("  ({n}, danneggiato)")
                } else {
                    format!("  ({n})")
                }
            }
            DocState::NeedsPassword { .. } => "  (password)".to_string(),
            DocState::NotPdf => "  (non è un PDF)".to_string(),
            DocState::Broken(_) => "  (danneggiato)".to_string(),
            DocState::Error(_) => "  (errore)".to_string(),
        };
        format!("{}. {}{}", i + 1, d.name, extra)
    }

    fn refresh_list(&mut self) {
        unsafe {
            let list = self.c.list;
            SendMessageW(list, WM_SETREDRAW, WPARAM(0), LPARAM(0));
            let top = SendMessageW(list, LB_GETTOPINDEX, WPARAM(0), LPARAM(0));
            SendMessageW(list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
            for (i, d) in self.docs.iter().enumerate() {
                let t = WStr::new(&Self::list_label(i, d));
                SendMessageW(list, LB_ADDSTRING, WPARAM(0), LPARAM(t.pcwstr().0 as isize));
            }
            SendMessageW(list, LB_SETTOPINDEX, WPARAM(top.0.max(0) as usize), LPARAM(0));
            let sel = self.current.map(|c| c as isize).unwrap_or(-1);
            SendMessageW(list, LB_SETCURSEL, WPARAM(sel as usize), LPARAM(0));
            SendMessageW(list, WM_SETREDRAW, WPARAM(1), LPARAM(0));
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(list, None, true);
        }
        let total: usize = self.docs.iter().map(|d| if let DocState::Ready(p, _) = &d.state { p.len() } else { 0 }).sum();
        let label = match self.docs.len() {
            0 => "Documenti (ordine di unione):".to_string(),
            1 => format!("1 documento, {total} pagine:"),
            n => format!("{n} documenti, {total} pagine (in quest'ordine):"),
        };
        set_text(self.c.lbl_list, &label);
    }

    /// Mostra il documento in posizione `i`.
    fn select(&mut self, i: usize) {
        if i >= self.docs.len() {
            return;
        }
        if let Some(cur) = self.current {
            if cur != i {
                if let Some(d) = self.docs.get_mut(cur) {
                    if Some(d.id) == self.viewer.doc {
                        d.view = self.viewer.save_view();
                    }
                }
            }
        }
        self.current = Some(i);
        unsafe {
            SendMessageW(self.c.list, LB_SETCURSEL, WPARAM(i), LPARAM(0));
        }
        self.show_current();
    }

    /// Mette nel visualizzatore il documento corrente, nello stato in cui è.
    fn show_current(&mut self) {
        let Some(i) = self.current else {
            self.viewer.show_message(None, HINT);
            self.sync_ui();
            return;
        };
        let d = &self.docs[i];
        let id = d.id;
        match &d.state {
            DocState::Loading => self.viewer.show_message(Some(id), &format!("Apertura di «{}»…", d.name)),
            DocState::Ready(sizes, damaged) => {
                if self.viewer.doc != Some(id) || !self.viewer.has_pages() {
                    let sizes = sizes.clone();
                    let view = d.view;
                    let damaged = *damaged;
                    self.viewer.set_document(Some(id), sizes, view, damaged);
                }
            }
            DocState::NotPdf => {
                let text = format!("«{}» non è un file PDF.\n\nQuesto formato non è supportato: myPDF apre solo documenti PDF.", d.name);
                self.viewer.show_message(Some(id), &text);
            }
            DocState::Broken(detail) => {
                let text = format!(
                    "Broken PDF format\n\n«{}» è un PDF danneggiato: non contiene pagine leggibili.\n\n({})",
                    d.name, detail
                );
                self.viewer.show_message(Some(id), &text);
            }
            DocState::NeedsPassword { .. } => {
                self.viewer.show_message(Some(id), &format!("«{}» è protetto da password.", d.name));
                if !self.asking_password {
                    self.asking_password = true;
                    post(self.hwnd, WM_APP_ASK_PASSWORD, id as usize, 0);
                }
            }
            DocState::Error(e) => {
                let text = format!("Impossibile aprire «{}».\n\n{}", d.name, e);
                self.viewer.show_message(Some(id), &text);
            }
        }
        self.sync_ui();
    }

    fn remove(&mut self, i: usize) {
        if i >= self.docs.len() {
            return;
        }
        let d = self.docs.remove(i);
        self.worker.close(d.id);
        self.viewer.forget_document(d.id);
        if self.viewer.doc == Some(d.id) {
            self.viewer.doc = None;
        }
        self.current = match self.current {
            Some(c) if c == i => {
                if self.docs.is_empty() {
                    None
                } else {
                    Some(i.min(self.docs.len() - 1))
                }
            }
            Some(c) if c > i => Some(c - 1),
            other => other,
        };
        self.refresh_list();
        self.show_current();
    }

    fn clear(&mut self) {
        for d in self.docs.drain(..) {
            self.worker.close(d.id);
            self.viewer.forget_document(d.id);
        }
        self.current = None;
        self.viewer.doc = None;
        self.refresh_list();
        self.show_current();
    }

    /// Sposta il documento `from` prima di quello in posizione `before` (`len` = in fondo).
    fn move_doc(&mut self, from: usize, before: usize) {
        if from >= self.docs.len() || before > self.docs.len() || before == from || before == from + 1 {
            return;
        }
        let cur_id = self.current.map(|c| self.docs[c].id);
        let d = self.docs.remove(from);
        let to = if before > from { before - 1 } else { before };
        self.docs.insert(to, d);
        self.current = cur_id.and_then(|id| self.doc_index(id));
        self.refresh_list();
    }

    fn move_current(&mut self, down: bool) {
        let Some(c) = self.current else { return };
        if down && c + 1 < self.docs.len() {
            self.move_doc(c, c + 2);
        } else if !down && c > 0 {
            self.move_doc(c, c - 1);
        }
    }

    fn sort_by_name(&mut self) {
        let cur_id = self.current.map(|c| self.docs[c].id);
        self.docs.sort_by(|a, b| natural_cmp(&a.name, &b.name));
        self.current = cur_id.and_then(|id| self.doc_index(id));
        self.refresh_list();
    }

    fn on_worker_events(&mut self) {
        for ev in self.worker.take_events() {
            match ev {
                worker::Event::Opened { doc, result } => {
                    let Some(i) = self.doc_index(doc) else { continue };
                    let d = &mut self.docs[i];
                    d.state = match result {
                        Ok(info) => DocState::Ready(info.sizes, info.damaged),
                        Err(LoadError::PasswordRequired) => DocState::NeedsPassword { wrong: d.password.is_some() },
                        Err(LoadError::NotPdf) => DocState::NotPdf,
                        Err(LoadError::Broken(e)) => DocState::Broken(e),
                        Err(LoadError::Other(e)) => DocState::Error(e),
                    };
                    self.refresh_list();
                    if self.current == Some(i) {
                        self.show_current();
                    }
                }
                worker::Event::Tile { key, result } => match result {
                    Ok(bmp) => self.viewer.add_tile(key, &bmp),
                    Err(_) => self.viewer.tile_failed(key),
                },
            }
        }
        self.sync_ui();
    }

    /// Aggiorna etichette, barra di stato, titolo e pulsanti attivi.
    fn sync_ui(&mut self) {
        let has = self.viewer.has_pages();
        let total = self.viewer.page_count();
        let cur = if has { self.viewer.current_page() + 1 } else { 0 };
        let zoom = self.viewer.zoom_percent();
        let shown = (cur, total, zoom);
        if self.shown != Some(shown) {
            unsafe {
                if GetFocus() != self.c.edit_page {
                    set_text(self.c.edit_page, &if has { cur.to_string() } else { String::new() });
                }
            }
            set_text(self.c.lbl_pages, &if has { format!("di {total}") } else { String::new() });
            set_text(self.c.lbl_zoom, &if has { format!("{zoom}%") } else { String::new() });
            self.shown = Some(shown);
        }
        let doc = self.current.and_then(|i| self.docs.get(i));
        let status = [
            self.status_msg.clone().unwrap_or_else(|| doc.map(|d| d.path.display().to_string()).unwrap_or_else(|| "Nessun documento".into())),
            if has { format!("Pagina {cur} di {total}") } else { String::new() },
            if has { format!("Zoom {zoom}%") } else { String::new() },
        ];
        for (i, s) in status.iter().enumerate() {
            if self.shown_status[i] != *s {
                let t = WStr::new(s);
                unsafe {
                    SendMessageW(self.c.status, SB_SETTEXTW, WPARAM(i), LPARAM(t.pcwstr().0 as isize));
                }
                self.shown_status[i] = s.clone();
            }
        }
        let title = match doc {
            Some(d) => format!("{} - myPDF", d.name),
            None => "myPDF".to_string(),
        };
        if title != self.shown_title {
            set_text(self.hwnd, &title);
            self.shown_title = title;
        }
        let ready = matches!(doc.map(|d| &d.state), Some(DocState::Ready(..)));
        let n = self.docs.len();
        let enable = |h: HWND, on: bool| unsafe {
            let _ = EnableWindow(h, on);
        };
        let c = &self.c;
        enable(c.btn_merge, n >= 2 && !self.merging);
        enable(c.btn_merge2, n >= 2 && !self.merging);
        enable(c.btn_print, ready && !self.printing);
        for h in [c.btn_zoom_out, c.btn_zoom_in, c.btn_fit_w, c.btn_fit_p, c.edit_page] {
            enable(h, has);
        }
        enable(c.btn_prev, has && cur > 1);
        enable(c.btn_next, has && cur < total);
        let sel = self.current;
        enable(c.btn_up, sel.is_some_and(|s| s > 0));
        enable(c.btn_down, sel.is_some_and(|s| s + 1 < n));
        enable(c.btn_remove, sel.is_some());
        enable(c.btn_clear, n > 0);
        unsafe {
            let menu = GetMenu(self.hwnd);
            let flag = |on: bool| if on { MF_ENABLED } else { MF_GRAYED };
            let _ = EnableMenuItem(menu, IDM_MERGE as u32, MF_BYCOMMAND | flag(n >= 2 && !self.merging));
            let _ = EnableMenuItem(menu, IDM_PRINT as u32, MF_BYCOMMAND | flag(ready && !self.printing));
            let _ = CheckMenuItem(menu, IDM_TOGGLE_PANEL as u32, (MF_BYCOMMAND | if self.panel_visible { MF_CHECKED } else { MF_UNCHECKED }).0);
        }
    }

    fn set_dpi(&mut self, dpi: u32) {
        self.dpi = dpi.max(96);
        self.viewer.dpi = self.dpi;
        self.apply_fonts();
        self.shown = None;
        self.layout();
        self.viewer.resized();
        self.sync_ui();
    }
}

/// Esegue un comando del menu, di una scorciatoia o di un pulsante.
fn command(hwnd: HWND, id: u16) {
    let viewer = hwnd_of(VIEWER.with(|v| v.get()));
    match id {
        IDM_OPEN => {
            let dir = with_app(|a| a.last_dir.clone()).flatten();
            let files = util::open_files_dialog(hwnd, dir.as_deref());
            if !files.is_empty() {
                with_app(|a| a.add_files(files, true));
            }
        }
        IDM_MERGE => cmd_merge(hwnd),
        IDM_PRINT => cmd_print(hwnd),
        IDM_CLOSE_DOC => {
            with_app(|a| {
                if let Some(c) = a.current {
                    a.remove(c)
                }
            });
        }
        IDM_CLOSE_ALL => {
            let n = with_app(|a| a.docs.len()).unwrap_or(0);
            if n > 1 && !ask_yes_no(hwnd, &format!("Togliere tutti i {n} documenti dall'elenco?\n(I file non vengono cancellati.)")) {
                return;
            }
            with_app(|a| a.clear());
        }
        IDM_EXIT => post(hwnd, WM_CLOSE, 0, 0),
        IDM_ZOOM_IN | IDM_ZOOM_OUT | IDM_ZOOM_100 | IDM_FIT_WIDTH | IDM_FIT_PAGE | IDM_PREV | IDM_NEXT | IDM_FIRST | IDM_LAST => {
            with_app(|a| {
                let v = &mut a.viewer;
                if !v.has_pages() {
                    return;
                }
                match id {
                    IDM_ZOOM_IN => v.zoom_step(true, None),
                    IDM_ZOOM_OUT => v.zoom_step(false, None),
                    IDM_ZOOM_100 => v.set_zoom(ZoomMode::Custom, 1.0, None),
                    IDM_FIT_WIDTH => v.set_zoom(ZoomMode::FitWidth, v.zoom, None),
                    IDM_FIT_PAGE => v.set_zoom(ZoomMode::FitPage, v.zoom, None),
                    IDM_PREV => {
                        let c = v.current_page();
                        v.go_to_page(c.saturating_sub(1));
                    }
                    IDM_NEXT => {
                        let c = v.current_page();
                        v.go_to_page(c + 1);
                    }
                    IDM_FIRST => v.go_to_page(0),
                    _ => v.go_to_page(usize::MAX),
                }
                a.sync_ui();
            });
        }
        IDM_GOTO => {
            if let Some(edit) = with_app(|a| a.c.edit_page) {
                unsafe {
                    let _ = SetFocus(edit);
                    SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1));
                }
            }
        }
        IDM_GOTO_FROM_EDIT => {
            with_app(|a| {
                if let Ok(n) = get_text(a.c.edit_page).trim().parse::<usize>() {
                    a.viewer.go_to_page(n.max(1) - 1);
                }
                a.shown = None;
                a.sync_ui();
            });
            unsafe {
                let _ = SetFocus(viewer);
            }
        }
        IDM_TOGGLE_PANEL => {
            with_app(|a| {
                a.panel_visible = !a.panel_visible;
                a.layout();
                a.sync_ui();
            });
        }
        IDM_MOVE_UP | IDM_MOVE_DOWN => {
            with_app(|a| {
                a.move_current(id == IDM_MOVE_DOWN);
                a.sync_ui();
            });
        }
        IDM_SORT => {
            with_app(|a| {
                a.sort_by_name();
                a.sync_ui();
            });
        }
        IDM_ASSOC => cmd_assoc(hwnd),
        IDM_UNASSOC => match std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| assoc::unregister(&exe)) {
            Ok(()) => message(hwnd, "myPDF non è più registrato per i file PDF.", MsgKind::Info),
            Err(e) => message(hwnd, &format!("Impossibile togliere la registrazione:\n{e}"), MsgKind::Error),
        },
        IDM_CLI_HELP => message(hwnd, &crate::cli::usage(), MsgKind::Info),
        IDM_ABOUT => message(
            hwnd,
            &format!(
                "myPDF {}\n\nVisualizzatore PDF minimale per Windows 10 e 11: anteprima, zoom, stampa e unione di più PDF in uno.\n\n\
                 Le pagine sono disegnate dal motore PDF integrato in Windows; l'unione usa la libreria lopdf.\n\n\
                 https://github.com/erik504/myPDF",
                crate::VERSION
            ),
            MsgKind::Info,
        ),
        _ => {}
    }
}

fn cmd_merge(hwnd: HWND) {
    let Some(info) = with_app(|a| {
        if a.merging {
            return None;
        }
        let paths: Vec<PathBuf> = a.docs.iter().map(|d| d.path.clone()).collect();
        let passwords: Vec<String> = a.docs.iter().filter_map(|d| d.password.clone()).collect();
        Some((paths, passwords))
    })
    .flatten() else {
        return;
    };
    let (paths, passwords) = info;
    if paths.len() < 2 {
        message(hwnd, "Per unire servono almeno due PDF nell'elenco.\nAggiungili con «Apri…» o trascinandoli nella finestra.", MsgKind::Info);
        return;
    }
    let dir = paths[0].parent().map(PathBuf::from).unwrap_or_default();
    let Some(out) = util::save_file_dialog(hwnd, &dir.join("PDF unito.pdf"), "Salva il PDF unito") else { return };
    let out = if out.extension().is_none() { out.with_extension("pdf") } else { out };
    let target = hwnd.0 as isize;
    with_app(|a| {
        a.merging = true;
        a.status_msg = Some(format!("Unione di {} documenti in corso…", paths.len()));
        let (tx, rx) = channel();
        a.merge_rx = Some(rx);
        let opts = MergeOptions { bookmarks: true, passwords };
        std::thread::spawn(move || {
            let r = merge_files(&paths, &out, &opts).map(|rep| (rep, out));
            let _ = tx.send(r);
            post(hwnd_of(target), WM_APP_MERGE_DONE, 0, 0);
        });
        a.sync_ui();
    });
}

fn on_merge_done(hwnd: HWND) {
    let res = with_app(|a| {
        a.merging = false;
        a.status_msg = None;
        a.sync_ui();
        a.merge_rx.take().and_then(|rx| rx.try_recv().ok())
    })
    .flatten();
    match res {
        Some(Ok((r, out))) => {
            let mut warn = String::new();
            if !r.repaired.is_empty() {
                warn.push_str("\n\nAttenzione: questi file erano danneggiati, sono state unite solo le pagine leggibili:");
                for f in &r.repaired {
                    warn.push_str(&format!("\n• {}", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                }
            }
            let text = format!("Creato «{}»\n{} documenti uniti, {} pagine in tutto.{warn}\n\nVuoi aprirlo?", out.display(), r.files, r.pages);
            if ask_yes_no(hwnd, &text) {
                with_app(|a| a.add_files(vec![out], true));
            }
        }
        Some(Err(e)) => message(hwnd, &format!("Unione non riuscita.\n\n{e}"), MsgKind::Error),
        None => {}
    }
}

fn cmd_print(hwnd: HWND) {
    let info = with_app(|a| {
        if a.printing {
            return Err("C'è già una stampa in corso.".to_string());
        }
        let Some(d) = a.current.and_then(|i| a.docs.get(i)) else { return Err("Apri prima un PDF.".into()) };
        let DocState::Ready(p, _) = &d.state else { return Err("Questo documento non si può stampare (non è pronto o non è un PDF leggibile).".into()) };
        Ok((d.path.clone(), d.password.clone(), d.name.clone(), p.len(), a.viewer.current_page()))
    });
    let (path, password, name, pages, current) = match info {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            message(hwnd, &e, MsgKind::Info);
            return;
        }
        None => return,
    };
    let mut settings = with_app(|a| std::mem::take(&mut a.printer)).unwrap_or_default();
    let opts = DialogOptions { page_count: Some(pages), current_page: Some(current), preset_pages: None, copies: 1 };
    let choice = print_dialog(hwnd, &mut settings, &opts);
    with_app(|a| a.printer = settings);
    let choice = match choice {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            message(hwnd, &e, MsgKind::Error);
            return;
        }
    };
    let target = hwnd.0 as isize;
    with_app(|a| {
        a.printing = true;
        a.status_msg = Some(format!("Stampa di «{name}»…"));
        let (tx, rx) = channel();
        a.print_rx = Some(rx);
        std::thread::spawn(move || {
            let r = std::fs::read(&path)
                .map_err(|e| format!("impossibile leggere il file ({e})"))
                .and_then(|bytes| {
                    let chosen = choice.pages.clone().unwrap_or_default(); // vuoto = tutte
                    print_document(&choice.dc, &bytes, password.as_deref(), &name, &chosen, None, &mut |done, total| {
                        post(hwnd_of(target), WM_APP_PRINT_PROGRESS, done, total as isize)
                    })
                });
            let _ = tx.send(r);
            post(hwnd_of(target), WM_APP_PRINT_DONE, 0, 0);
        });
        a.sync_ui();
    });
}

fn on_print_done(hwnd: HWND) {
    let res = with_app(|a| {
        a.printing = false;
        a.status_msg = None;
        a.sync_ui();
        a.print_rx.take().and_then(|rx| rx.try_recv().ok())
    })
    .flatten();
    if let Some(Err(e)) = res {
        message(hwnd, &format!("Stampa non riuscita.\n\n{e}"), MsgKind::Error);
    }
}

fn cmd_assoc(hwnd: HWND) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            message(hwnd, &format!("Percorso del programma sconosciuto: {e}"), MsgKind::Error);
            return;
        }
    };
    match assoc::register(&exe) {
        Ok(()) => {
            message(
                hwnd,
                &format!(
                    "myPDF è ora tra i programmi che possono aprire i PDF (percorso: {}).\n\n\
                     Windows non permette ai programmi di diventare predefiniti da soli: nella finestra \
                     «App predefinite» che si apre ora, scegli myPDF per «.pdf».\n\n\
                     Se sposti myPDF.exe in un'altra cartella, ripeti questo comando.",
                    exe.display()
                ),
                MsgKind::Info,
            );
            assoc::open_default_apps_settings();
        }
        Err(e) => message(hwnd, &format!("Registrazione non riuscita:\n{e}"), MsgKind::Error),
    }
}

fn ask_password(hwnd: HWND, id: u64) {
    let info = with_app(|a| a.doc_index(id).map(|i| (a.docs[i].name.clone(), matches!(a.docs[i].state, DocState::NeedsPassword { wrong: true }))));
    let Some(Some((name, wrong))) = info else {
        with_app(|a| a.asking_password = false);
        return;
    };
    let pw = prompt::ask_password(hwnd, &name, wrong);
    with_app(|a| {
        a.asking_password = false;
        let Some(i) = a.doc_index(id) else { return };
        match pw {
            Some(pw) => {
                let d = &mut a.docs[i];
                d.password = Some(pw.clone());
                d.state = DocState::Loading;
                a.worker.open(id, d.path.clone(), Some(pw));
            }
            None => a.docs[i].state = DocState::Error("Il PDF è protetto da password e la password non è stata inserita.".into()),
        }
        a.refresh_list();
        if a.current == Some(i) {
            a.show_current();
        }
    });
}

fn dropped_files(hdrop: HDROP) -> Vec<PathBuf> {
    let mut out = Vec::new();
    unsafe {
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        for i in 0..n {
            let len = DragQueryFileW(hdrop, i, None);
            let mut buf = vec![0u16; len as usize + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            out.push(path_from_wide(&buf));
        }
        DragFinish(hdrop);
    }
    out
}

fn in_splitter(a: &App, x: i32, y: i32) -> bool {
    let p = a.panel_px();
    let top = a.px(42);
    a.panel_visible && x >= p - a.px(6) && x < p + a.px(SPLITTER) && y >= top
}

/// Le procedure di finestra non devono mai far chiudere il programma: un panico (errore
/// interno inatteso) viene intercettato e il messaggio trattato come gestito.
fn guarded(f: impl FnOnce() -> LRESULT) -> LRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(LRESULT(0))
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    guarded(|| main_proc_impl(hwnd, msg, wparam, lparam))
}

unsafe extern "system" fn viewer_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    guarded(|| viewer_proc_impl(hwnd, msg, wparam, lparam))
}

unsafe fn main_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let drag_msg = DRAG_MSG.with(|m| m.get());
    if drag_msg != 0 && msg == drag_msg {
        return on_drag_list(hwnd, lparam);
    }
    match msg {
        WM_SIZE | WM_APP_RELAYOUT => {
            if with_app(|a| {
                a.layout();
                a.sync_ui();
            })
            .is_none()
                && msg == WM_SIZE
            {
                post(hwnd, WM_APP_RELAYOUT, 0, 0);
            }
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
            mmi.ptMinTrackSize = POINT { x: 640 * dpi / 96, y: 420 * dpi / 96 };
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = loword(wparam.0);
            let code = hiword(wparam.0) as u32;
            if lparam.0 != 0 {
                // notifiche dei controlli
                if id == ID_LIST {
                    if code == LBN_SELCHANGE {
                        with_app(|a| {
                            let sel = SendMessageW(a.c.list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                            if sel >= 0 {
                                a.select(sel as usize);
                            }
                        });
                    }
                    return LRESULT(0);
                }
                if id == ID_EDIT_PAGE || code != BN_CLICKED {
                    return LRESULT(0);
                }
                command(hwnd, id);
                // dopo un pulsante la tastiera torna alle pagine (o all'elenco per i suoi pulsanti)
                let back = if matches!(id, IDM_MOVE_UP | IDM_MOVE_DOWN | IDM_CLOSE_DOC | IDM_CLOSE_ALL) {
                    with_app(|a| a.c.list)
                } else {
                    Some(hwnd_of(VIEWER.with(|v| v.get())))
                };
                if let Some(b) = back {
                    if IsWindowEnabled(hwnd).as_bool() {
                        let _ = SetFocus(b);
                    }
                }
                return LRESULT(0);
            }
            command(hwnd, id);
            LRESULT(0)
        }
        WM_DROPFILES => {
            let files = dropped_files(HDROP(wparam.0 as *mut _));
            let _ = SetForegroundWindow(hwnd);
            with_app(|a| a.add_files(files, true));
            LRESULT(0)
        }
        WM_APP_WORKER => {
            with_app(|a| a.on_worker_events());
            LRESULT(0)
        }
        WM_APP_ASK_PASSWORD => {
            ask_password(hwnd, wparam.0 as u64);
            LRESULT(0)
        }
        WM_APP_MERGE_DONE => {
            on_merge_done(hwnd);
            LRESULT(0)
        }
        WM_APP_PRINT_PROGRESS => {
            with_app(|a| {
                if let Some(d) = a.current.and_then(|i| a.docs.get(i)) {
                    let _ = d;
                }
                a.status_msg = Some(format!("Stampa in corso: pagina {} di {}…", wparam.0, lparam.0));
                a.sync_ui();
            });
            LRESULT(0)
        }
        WM_APP_PRINT_DONE => {
            on_print_done(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let rc = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(hwnd, None, rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER | SWP_NOACTIVATE);
            let dpi = hiword(wparam.0) as u32;
            with_app(|a| a.set_dpi(dpi));
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if HWND(wparam.0 as *mut _) == hwnd && loword(lparam.0 as usize) as u32 == HTCLIENT {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                if with_app(|a| in_splitter(a, pt.x, pt.y) || a.splitter.is_some()) == Some(true) {
                    SetCursor(LoadCursorW(None, IDC_SIZEWE).unwrap_or_default());
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = point_from_lparam(lparam);
            let start = with_app(|a| {
                if in_splitter(a, x, y) {
                    a.splitter = Some(x - a.panel_px());
                    true
                } else {
                    false
                }
            });
            if start == Some(true) {
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, _) = point_from_lparam(lparam);
            with_app(|a| {
                if let Some(off) = a.splitter {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let px = (x - off).clamp(a.px(150), (rc.right - a.px(300)).max(a.px(150)));
                    let w96 = px * 96 / a.dpi as i32;
                    if w96 != a.panel_w96 {
                        a.panel_w96 = w96;
                        a.layout();
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            let was = with_app(|a| a.splitter.take().is_some()).unwrap_or(false);
            if was && msg == WM_LBUTTONUP {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if loword(wparam.0) as u32 == WA_INACTIVE {
                let f = GetFocus();
                if !f.0.is_null() {
                    LAST_FOCUS.with(|l| l.set(f.0 as isize));
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SETFOCUS => {
            let last = hwnd_of(LAST_FOCUS.with(|l| l.get()));
            let target = if !last.0.is_null() && IsWindow(last).as_bool() && IsChild(hwnd, last).as_bool() { last } else { hwnd_of(VIEWER.with(|v| v.get())) };
            let _ = SetFocus(target);
            LRESULT(0)
        }
        WM_CLOSE => {
            let printing = with_app(|a| a.printing || a.merging).unwrap_or(false);
            if printing && !ask_yes_no(hwnd, "C'è una stampa o un'unione in corso: uscendo verrà interrotta.\nUscire comunque?") {
                return LRESULT(0);
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // Lo stato (thread di disegno compreso) si chiude fuori dal prestito.
            let app = APP.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.take()));
            if let Some(app) = app {
                let font = app.font;
                drop(app);
                if !font.is_invalid() {
                    let _ = DeleteObject(font);
                }
            }
            MAIN.with(|m| m.set(0));
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Riordino dell'elenco trascinando (DragListBox di Windows).
unsafe fn on_drag_list(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let info = &*(lparam.0 as *const DRAGLISTINFO);
    let list = info.hWnd;
    match info.uNotification {
        DL_BEGINDRAG => {
            let item = LBItemFromPt(list, info.ptCursor, false);
            with_app(|a| a.drag_from = (item >= 0).then_some(item as usize));
            LRESULT((item >= 0) as isize)
        }
        DL_DRAGGING => {
            let item = LBItemFromPt(list, info.ptCursor, true);
            DrawInsert(hwnd, list, item);
            LRESULT(if item >= 0 || cursor_in(list, info.ptCursor) { DL_MOVECURSOR as isize } else { DL_STOPCURSOR as isize })
        }
        DL_DROPPED => {
            let mut item = LBItemFromPt(list, info.ptCursor, false);
            DrawInsert(hwnd, list, -1);
            if item < 0 && cursor_in(list, info.ptCursor) {
                item = i32::MAX;
            }
            with_app(|a| {
                if let (Some(from), true) = (a.drag_from.take(), item >= 0) {
                    let before = (item as usize).min(a.docs.len());
                    a.move_doc(from, before);
                    a.sync_ui();
                }
            });
            LRESULT(0)
        }
        DL_CANCELDRAG => {
            DrawInsert(hwnd, list, -1);
            with_app(|a| a.drag_from = None);
            LRESULT(0)
        }
        _ => LRESULT(0),
    }
}

unsafe fn cursor_in(hwnd: HWND, pt_screen: POINT) -> bool {
    let mut rc = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rc);
    pt_screen.x >= rc.left && pt_screen.x < rc.right && pt_screen.y >= rc.top && pt_screen.y < rc.bottom
}

/// Invio nella casella del numero di pagina: vai a quella pagina; Esc torna alle pagine.
unsafe extern "system" fn edit_subclass(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_KEYDOWN if wparam.0 == VK_RETURN.0 as usize => {
            post(GetParent(hwnd).unwrap_or_default(), WM_COMMAND, IDM_GOTO_FROM_EDIT as usize, 0);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
            with_app(|a| {
                a.shown = None;
                a.sync_ui();
            });
            let _ = SetFocus(hwnd_of(VIEWER.with(|v| v.get())));
            LRESULT(0)
        }
        WM_CHAR if wparam.0 == 13 || wparam.0 == 27 => LRESULT(0),
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

fn scroll_code(v: &Viewer, code: u32, vertical: bool) -> Option<i32> {
    let (pos, page, line) = if vertical { (v.sy, v.ch, v.line_px()) } else { (v.sx, v.cw, v.line_px()) };
    let bar = if vertical { SB_VERT } else { SB_HORZ };
    Some(match SCROLLBAR_COMMAND(code as i32) {
        SB_LINEUP => pos - line,
        SB_LINEDOWN => pos + line,
        SB_PAGEUP => pos - (page - line).max(line),
        SB_PAGEDOWN => pos + (page - line).max(line),
        SB_TOP => 0,
        SB_BOTTOM => i32::MAX / 2,
        SB_THUMBTRACK | SB_THUMBPOSITION => {
            let mut si = SCROLLINFO { cbSize: std::mem::size_of::<SCROLLINFO>() as u32, fMask: SIF_TRACKPOS, ..Default::default() };
            unsafe {
                let _ = GetScrollInfo(v.hwnd, bar, &mut si);
            }
            si.nTrackPos
        }
        _ => return None,
    })
}

unsafe fn viewer_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let done = with_app(|a| a.viewer.paint(hdc, &a.worker)).is_some();
            let _ = EndPaint(hwnd, &ps);
            if !done {
                post(hwnd, WM_APP_VIEW_LAYOUT, 0, 0);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE | WM_APP_VIEW_LAYOUT => {
            if with_app(|a| {
                a.viewer.resized();
                a.sync_ui();
            })
            .is_none()
                && msg == WM_SIZE
            {
                post(hwnd, WM_APP_VIEW_LAYOUT, 0, 0);
            }
            LRESULT(0)
        }
        WM_VSCROLL | WM_HSCROLL => {
            let vertical = msg == WM_VSCROLL;
            with_app(|a| {
                if let Some(p) = scroll_code(&a.viewer, loword(wparam.0) as u32, vertical) {
                    if vertical {
                        a.viewer.scroll_to(a.viewer.sx, p);
                    } else {
                        a.viewer.scroll_to(p, a.viewer.sy);
                    }
                    a.sync_ui();
                }
            });
            LRESULT(0)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = hiword(wparam.0) as i16 as i32;
            let keys = loword(wparam.0) as u32;
            let (sx, sy) = point_from_lparam(lparam);
            let mut pt = POINT { x: sx, y: sy };
            let _ = ScreenToClient(hwnd, &mut pt);
            let mut lines = 3u32;
            let _ = SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, Some(&mut lines as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
            with_app(|a| {
                let v = &mut a.viewer;
                if msg == WM_MOUSEWHEEL && keys & MK_CONTROL.0 != 0 {
                    if v.has_pages() {
                        v.zoom_by(1.1f64.powf(delta as f64 / 120.0), (pt.x, pt.y));
                    }
                } else {
                    let horizontal = msg == WM_MOUSEHWHEEL || keys & MK_SHIFT.0 != 0;
                    let step = if lines == u32::MAX { if horizontal { v.cw } else { v.ch } } else { v.line_px() * lines.max(1) as i32 };
                    let amount = delta * step / 120;
                    if msg == WM_MOUSEHWHEEL {
                        v.scroll_by(amount, 0);
                    } else if horizontal {
                        v.scroll_by(-amount, 0);
                    } else {
                        v.scroll_by(0, -amount);
                    }
                }
                a.sync_ui();
            });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = wparam.0 as u16;
            let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            let handled = with_app(|a| {
                let v = &mut a.viewer;
                let line = v.line_px();
                let page = (v.ch - line).max(line);
                let k = VIRTUAL_KEY(key);
                let done = match k {
                    VK_UP => {
                        v.scroll_by(0, -line);
                        true
                    }
                    VK_DOWN => {
                        v.scroll_by(0, line);
                        true
                    }
                    VK_PRIOR => {
                        v.scroll_by(0, -page);
                        true
                    }
                    VK_NEXT => {
                        v.scroll_by(0, page);
                        true
                    }
                    VK_SPACE => {
                        v.scroll_by(0, if shift { -page } else { page });
                        true
                    }
                    VK_HOME => {
                        v.scroll_to(if ctrl { 0 } else { v.sx }, 0);
                        true
                    }
                    VK_END => {
                        v.scroll_to(v.sx, i32::MAX / 2);
                        true
                    }
                    VK_LEFT | VK_RIGHT => {
                        let right = k == VK_RIGHT;
                        if v.can_scroll_x() {
                            v.scroll_by(if right { line } else { -line }, 0);
                        } else if v.has_pages() {
                            let c = v.current_page();
                            v.go_to_page(if right { c + 1 } else { c.saturating_sub(1) });
                        }
                        true
                    }
                    _ => false,
                };
                if done {
                    a.sync_ui();
                }
                done
            });
            if handled == Some(true) {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(hwnd);
            let (x, y) = point_from_lparam(lparam);
            let started = with_app(|a| {
                if a.viewer.has_pages() {
                    a.viewer.drag = Some((x, y, a.viewer.sx, a.viewer.sy));
                    true
                } else {
                    false
                }
            });
            if started == Some(true) {
                SetCapture(hwnd);
                SetCursor(LoadCursorW(None, IDC_SIZEALL).unwrap_or_default());
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let _ = SetFocus(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = point_from_lparam(lparam);
            with_app(|a| {
                if let Some((x0, y0, sx0, sy0)) = a.viewer.drag {
                    a.viewer.scroll_to(sx0 - (x - x0), sy0 - (y - y0));
                    a.sync_ui();
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            with_app(|a| a.viewer.drag = None);
            let _ = ReleaseCapture();
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            with_app(|a| a.viewer.drag = None);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if loword(lparam.0 as usize) as u32 == HTCLIENT {
                let dragging = with_app(|a| a.viewer.drag.is_some()).unwrap_or(false);
                SetCursor(LoadCursorW(None, if dragging { IDC_SIZEALL } else { IDC_ARROW }).unwrap_or_default());
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_GETDLGCODE => LRESULT((DLGC_WANTARROWS | DLGC_WANTCHARS) as isize),
        WM_DROPFILES => {
            let main = hwnd_of(MAIN.with(|m| m.get()));
            SendMessageW(main, WM_DROPFILES, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
