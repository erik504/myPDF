//! Thread che apre i PDF e disegna le pagine, così l'interfaccia non si blocca mai.
//!
//! L'interfaccia sostituisce di continuo l'elenco dei riquadri che le servono ([`Worker::set_wants`]):
//! quelli non più visibili vengono scartati prima di essere disegnati. I risultati vengono
//! messi in coda e la finestra viene avvisata con un messaggio.

use crate::render::{init_thread, open_document, Bitmap, LoadError, PdfDoc};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Un riquadro di pagina a una certa scala.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub doc: u64,
    pub page: u32,
    /// Scala (pixel per DIP) × 4096, arrotondata.
    pub scale: u32,
    pub tx: u16,
    pub ty: u16,
}

/// Richiesta di disegno di un riquadro.
#[derive(Clone, Debug)]
pub struct TileReq {
    pub key: TileKey,
    /// Parte della pagina in DIP (x, y, larghezza, altezza); `None` = pagina intera.
    pub source: Option<(f32, f32, f32, f32)>,
    pub width: u32,
    pub height: u32,
}

/// Un documento aperto: dimensioni delle pagine (DIP) e se il file era danneggiato.
pub struct OpenInfo {
    pub sizes: Vec<(f32, f32)>,
    pub damaged: bool,
}

pub enum Event {
    Opened { doc: u64, result: Result<OpenInfo, LoadError> },
    Tile { key: TileKey, result: Result<Bitmap, String> },
}

enum Cmd {
    Open { doc: u64, path: PathBuf, password: Option<String> },
    Close { doc: u64 },
}

#[derive(Default)]
struct Queue {
    cmds: VecDeque<Cmd>,
    wants: VecDeque<TileReq>,
    in_flight: Option<TileKey>,
    events: Vec<Event>,
    notified: bool,
    quit: bool,
}

struct Shared {
    q: Mutex<Queue>,
    cv: Condvar,
}

pub struct Worker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// HWND da passare al thread (serve solo per PostMessage, che si può chiamare da ogni thread).
#[derive(Clone, Copy)]
struct NotifyTarget(isize, u32);
unsafe impl Send for NotifyTarget {}

impl Worker {
    /// Avvia il thread; a ogni nuovo risultato manda `msg` a `hwnd`.
    pub fn start(hwnd: HWND, msg: u32) -> Worker {
        let shared = Arc::new(Shared { q: Mutex::new(Queue::default()), cv: Condvar::new() });
        let s2 = shared.clone();
        let target = NotifyTarget(hwnd.0 as isize, msg);
        let thread = std::thread::Builder::new().name("render".into()).spawn(move || run(s2, target)).expect("thread di rendering");
        Worker { shared, thread: Some(thread) }
    }

    pub fn open(&self, doc: u64, path: PathBuf, password: Option<String>) {
        let mut q = self.shared.q.lock().unwrap();
        q.cmds.push_back(Cmd::Open { doc, path, password });
        self.shared.cv.notify_one();
    }

    pub fn close(&self, doc: u64) {
        let mut q = self.shared.q.lock().unwrap();
        q.wants.retain(|r| r.key.doc != doc);
        q.cmds.push_back(Cmd::Close { doc });
        self.shared.cv.notify_one();
    }

    /// Sostituisce l'elenco dei riquadri da disegnare (in ordine di priorità). Quelli già in
    /// lavorazione o già pronti ma non ancora ritirati non vengono richiesti di nuovo.
    pub fn set_wants(&self, wants: Vec<TileReq>) {
        let mut q = self.shared.q.lock().unwrap();
        let mut pending: HashSet<TileKey> = q
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Tile { key, .. } => Some(*key),
                _ => None,
            })
            .collect();
        if let Some(k) = q.in_flight {
            pending.insert(k);
        }
        q.wants = wants.into_iter().filter(|r| !pending.contains(&r.key)).collect();
        if !q.wants.is_empty() {
            self.shared.cv.notify_one();
        }
    }

    /// Ritira i risultati pronti.
    pub fn take_events(&self) -> Vec<Event> {
        let mut q = self.shared.q.lock().unwrap();
        q.notified = false;
        std::mem::take(&mut q.events)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        {
            let mut q = self.shared.q.lock().unwrap();
            q.quit = true;
            q.wants.clear();
            q.cmds.clear();
        }
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(shared: Arc<Shared>, target: NotifyTarget) {
    init_thread();
    let mut docs: HashMap<u64, PdfDoc> = HashMap::new();
    loop {
        enum Job {
            Cmd(Cmd),
            Tile(TileReq),
        }
        let job = {
            let mut q = shared.q.lock().unwrap();
            loop {
                if q.quit {
                    return;
                }
                if let Some(c) = q.cmds.pop_front() {
                    break Job::Cmd(c);
                }
                if let Some(r) = q.wants.pop_front() {
                    q.in_flight = Some(r.key);
                    break Job::Tile(r);
                }
                q = shared.cv.wait(q).unwrap();
            }
        };
        let event = match job {
            Job::Cmd(Cmd::Open { doc, path, password }) => {
                let result = std::fs::read(&path).map_err(|e| LoadError::Other(format!("impossibile leggere il file ({e})"))).and_then(|bytes| {
                    // qualsiasi file, anche rovinato o di altro tipo: mai una chiusura del programma
                    std::panic::catch_unwind(|| open_document(&bytes, password.as_deref()))
                        .unwrap_or_else(|_| Err(LoadError::Broken("errore interno nella lettura del file".into())))
                });
                let result = result.map(|o| {
                    let info = OpenInfo { sizes: o.doc.page_sizes().to_vec(), damaged: o.damaged };
                    docs.insert(doc, o.doc);
                    info
                });
                Some(Event::Opened { doc, result })
            }
            Job::Cmd(Cmd::Close { doc }) => {
                docs.remove(&doc);
                None
            }
            Job::Tile(req) => {
                let result = match docs.get(&req.key.doc) {
                    Some(d) => {
                        // Un errore inatteso del motore non deve chiudere il programma.
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.render(req.key.page as usize, req.source, req.width, req.height)))
                            .unwrap_or_else(|_| Err("errore interno nel disegno della pagina".into()))
                    }
                    None => Err("documento chiuso".into()),
                };
                Some(Event::Tile { key: req.key, result })
            }
        };
        let mut q = shared.q.lock().unwrap();
        q.in_flight = None;
        if let Some(e) = event {
            q.events.push(e);
            if !q.notified {
                q.notified = true;
                unsafe {
                    let _ = PostMessageW(HWND(target.0 as *mut _), target.1, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
}
