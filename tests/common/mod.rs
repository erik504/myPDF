//! PDF di prova generati al volo.
#![allow(dead_code)]

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};

/// Una pagina: MediaBox (larghezza, altezza in punti), CropBox opzionale, rotazione, contenuto.
pub struct PageSpec {
    pub media: (f32, f32),
    pub crop: Option<[f32; 4]>,
    pub rotate: i64,
    pub ops: Vec<Operation>,
}

pub fn rect(r: f32, g: f32, b: f32, x: f32, y: f32, w: f32, h: f32) -> Vec<Operation> {
    vec![
        Operation::new("rg", vec![r.into(), g.into(), b.into()]),
        Operation::new("re", vec![x.into(), y.into(), w.into(), h.into()]),
        Operation::new("f", vec![]),
    ]
}

pub fn text(label: &str) -> Vec<Operation> {
    vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec!["F1".into(), 24.into()]),
        Operation::new("Td", vec![40.into(), 40.into()]),
        Operation::new("Tj", vec![Object::string_literal(label)]),
        Operation::new("ET", vec![]),
    ]
}

pub fn build(pages: Vec<PageSpec>) -> Document {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let resources = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font } });
    let mut kids = Vec::new();
    for p in pages {
        let content = Content { operations: p.ops };
        let cid = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => cid,
            "Resources" => resources,
            "MediaBox" => vec![0.into(), 0.into(), p.media.0.into(), p.media.1.into()],
        };
        if let Some(c) = p.crop {
            page.set("CropBox", c.iter().map(|&v| Object::from(v)).collect::<Vec<_>>());
        }
        if p.rotate != 0 {
            page.set("Rotate", p.rotate);
        }
        kids.push(Object::Reference(doc.add_object(page)));
    }
    let n = kids.len() as i64;
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => n }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    doc
}

pub fn bytes(mut doc: Document) -> Vec<u8> {
    let mut v = Vec::new();
    doc.save_to(&mut v).unwrap();
    v
}

/// PDF con una pagina di testo per ogni etichetta.
pub fn labeled(labels: &[&str]) -> Vec<u8> {
    bytes(build(labels.iter().map(|l| PageSpec { media: (595.0, 842.0), crop: None, rotate: 0, ops: text(l) }).collect()))
}

/// Testi delle pagine di un PDF (con lopdf).
pub fn page_texts(pdf: &[u8]) -> Vec<String> {
    let doc = Document::load_mem(pdf).unwrap();
    let n = doc.get_pages().len() as u32;
    (1..=n).map(|p| doc.extract_text(&[p]).unwrap_or_default().trim().to_string()).collect()
}

/// Cartella temporanea che si cancella da sola.
pub struct TempDir(pub std::path::PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("mypdf-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
