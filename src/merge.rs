//! Unione di più PDF in un unico file, nell'ordine dato.
//!
//! Ogni documento viene caricato con lopdf, i suoi oggetti rinumerati per non sovrapporsi a
//! quelli degli altri e le sue pagine appese a un nuovo albero delle pagine. Gli attributi che
//! le pagine ereditano dai nodi dell'albero originale (risorse, MediaBox, CropBox, rotazione)
//! vengono copiati sulle pagine, così l'aspetto resta identico. Si conservano anche i campi
//! dei moduli e i livelli (contenuto opzionale); per ogni file si aggiunge un segnalibro.

use lopdf::{Bookmark, Dictionary, Document, LoadOptions, Object, ObjectId};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Attributi delle pagine ereditabili dai nodi `/Pages` (PDF 32000, tabella 31).
const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// Opzioni dell'unione.
#[derive(Clone, Debug)]
pub struct MergeOptions {
    /// Un segnalibro per ogni file unito, con il nome del file.
    pub bookmarks: bool,
    /// Password da provare sui PDF protetti.
    pub passwords: Vec<String>,
}

impl Default for MergeOptions {
    fn default() -> Self {
        MergeOptions { bookmarks: true, passwords: Vec::new() }
    }
}

/// Esito di un'unione riuscita.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeReport {
    pub files: usize,
    pub pages: usize,
    /// File danneggiati di cui sono state unite solo le pagine leggibili.
    pub repaired: Vec<PathBuf>,
}

/// Errore dell'unione, con il file che l'ha causato se si sa.
#[derive(Debug)]
pub struct MergeError {
    pub file: Option<PathBuf>,
    pub message: String,
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.file {
            Some(p) => write!(f, "{}: {}", p.display(), self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for MergeError {}

/// Carica un PDF da memoria provando, se è cifrato, nessuna password e poi quelle date.
/// Un PDF danneggiato o troncato viene recuperato (solo le pagine leggibili): in quel caso
/// il secondo valore è vero.
pub fn load_document(bytes: &[u8], passwords: &[String]) -> Result<(Document, bool), String> {
    if !crate::repair::looks_like_pdf(bytes) {
        return Err("non è un file PDF".into());
    }
    let first = match Document::load_mem(bytes) {
        Ok(d) if d.is_encrypted() || d.page_iter().any(|p| d.get_dictionary(p).is_ok()) => d,
        Ok(_) | Err(_) => {
            return match crate::repair::salvage_doc(bytes) {
                Some(d) => Ok((d, true)),
                None => Err("PDF danneggiato: nessuna pagina leggibile (broken PDF format)".into()),
            }
        }
    };
    if !first.is_encrypted() {
        return Ok((first, false));
    }
    for pw in passwords {
        if let Ok(doc) = Document::load_mem_with_options(bytes, LoadOptions::with_password(pw)) {
            if !doc.is_encrypted() {
                return Ok((doc, false));
            }
        }
    }
    Err(if passwords.is_empty() {
        "il PDF è protetto da password".into()
    } else {
        "il PDF è protetto e nessuna delle password date è giusta".into()
    })
}

/// Unisce i file `inputs` (nell'ordine) e scrive il risultato in `output`.
///
/// Il file di uscita viene scritto prima con un nome temporaneo e poi rinominato: un errore
/// non lascia mai un PDF a metà, e l'uscita può anche coincidere con uno dei file uniti.
pub fn merge_files(inputs: &[PathBuf], output: &Path, opts: &MergeOptions) -> Result<MergeReport, MergeError> {
    // Un errore inatteso di lopdf su un file strano diventa un errore, non una chiusura.
    std::panic::catch_unwind(|| merge_files_inner(inputs, output, opts))
        .unwrap_or_else(|_| Err(MergeError { file: None, message: "errore interno durante l'unione (file danneggiato?)".into() }))
}

fn merge_files_inner(inputs: &[PathBuf], output: &Path, opts: &MergeOptions) -> Result<MergeReport, MergeError> {
    if inputs.is_empty() {
        return Err(MergeError { file: None, message: "nessun file da unire".into() });
    }
    let mut docs = Vec::with_capacity(inputs.len());
    let mut repaired = Vec::new();
    for path in inputs {
        let err = |message: String| MergeError { file: Some(path.clone()), message };
        let bytes = std::fs::read(path).map_err(|e| err(format!("impossibile leggere il file ({e})")))?;
        let (doc, fixed) = load_document(&bytes, &opts.passwords).map_err(err)?;
        if fixed {
            repaired.push(path.clone());
        }
        docs.push((display_title(path), doc));
    }
    let mut merged = merge_documents(docs, opts.bookmarks).map_err(|message| MergeError { file: None, message })?;
    let pages = merged.get_pages().len();
    save_atomic(&mut merged, output).map_err(|message| MergeError { file: Some(output.to_path_buf()), message })?;
    Ok(MergeReport { files: inputs.len(), pages, repaired })
}

/// Titolo del segnalibro di un file: il nome senza estensione.
pub fn display_title(path: &Path) -> String {
    path.file_stem().or(path.file_name()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "PDF".into())
}

/// Salva `doc` in `output` passando da un file temporaneo nella stessa cartella.
pub fn save_atomic(doc: &mut Document, output: &Path) -> Result<(), String> {
    let dir = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = output.file_name().ok_or("nome del file di uscita non valido")?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);
    let mut write = || -> Result<(), String> {
        let file = std::fs::File::create(&tmp).map_err(|e| format!("impossibile creare il file ({e})"))?;
        let mut w = std::io::BufWriter::new(file);
        doc.save_to(&mut w).map_err(|e| format!("errore di scrittura ({e})"))?;
        use std::io::Write;
        w.flush().map_err(|e| format!("errore di scrittura ({e})"))?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, output) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("impossibile sostituire il file, forse è aperto in un altro programma ({e})"));
    }
    Ok(())
}

/// Unisce documenti già caricati (titolo per il segnalibro, documento), nell'ordine dato.
pub fn merge_documents(docs: Vec<(String, Document)>, bookmarks: bool) -> Result<Document, String> {
    let mut out = Document::with_version("1.7");
    let mut next_id: u32 = 1;
    let mut all_pages: Vec<ObjectId> = Vec::new();
    let mut first_pages: Vec<(String, ObjectId)> = Vec::new();
    let mut form = FormMerge::default();
    let mut layers = LayerMerge::default();
    let mut version = 1.4f32;

    for (title, mut doc) in docs {
        if let Ok(v) = doc.version.trim().parse::<f32>() {
            version = version.max(v);
        }
        next_id = renumber(&mut doc, next_id);
        // le pagine il cui oggetto manca (file danneggiato) si saltano
        let pages: Vec<ObjectId> = doc.page_iter().filter(|p| doc.get_dictionary(*p).is_ok()).collect();
        if pages.is_empty() {
            continue;
        }
        for &p in &pages {
            inherit_attributes(&mut doc, p);
        }
        let catalog_id = doc.trailer.get(b"Root").and_then(Object::as_reference).ok();
        let tree_nodes = page_tree_nodes(&doc);
        if let Ok(catalog) = doc.catalog() {
            form.collect(&doc, catalog);
            layers.collect(&doc, catalog);
        }
        for (id, obj) in std::mem::take(&mut doc.objects) {
            if Some(id) == catalog_id || tree_nodes.contains(&id) {
                continue;
            }
            out.objects.insert(id, obj);
        }
        first_pages.push((title, pages[0]));
        all_pages.extend(pages);
    }

    if all_pages.is_empty() {
        return Err("i file scelti non contengono pagine".into());
    }

    out.version = if version >= 2.0 { "2.0".into() } else { "1.7".into() };
    out.max_id = next_id;
    let pages_id = out.new_object_id();
    let mut seen = HashSet::new();
    for &p in &all_pages {
        // La stessa pagina due volte nello stesso documento: la seconda diventa una copia.
        if !seen.insert(p) {
            continue;
        }
        if let Ok(dict) = out.get_object_mut(p).and_then(Object::as_dict_mut) {
            dict.set("Parent", Object::Reference(pages_id));
            dict.set("Type", Object::Name(b"Page".to_vec()));
        }
    }
    let mut kids = Vec::with_capacity(all_pages.len());
    let mut used = HashSet::new();
    for &p in &all_pages {
        if used.insert(p) {
            kids.push(Object::Reference(p));
        } else if let Ok(obj) = out.get_object(p).cloned() {
            let copy = out.add_object(obj);
            kids.push(Object::Reference(copy));
        }
    }
    let count = kids.len() as i64;
    let mut pages = Dictionary::new();
    pages.set("Type", Object::Name(b"Pages".to_vec()));
    pages.set("Kids", Object::Array(kids));
    pages.set("Count", Object::Integer(count));
    out.objects.insert(pages_id, Object::Dictionary(pages));

    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));
    if let Some(acro) = form.build() {
        let id = out.add_object(Object::Dictionary(acro));
        catalog.set("AcroForm", Object::Reference(id));
    }
    if let Some(oc) = layers.build() {
        catalog.set("OCProperties", Object::Dictionary(oc));
    }
    if bookmarks && first_pages.len() > 1 {
        for (title, page) in &first_pages {
            out.add_bookmark(Bookmark::new(title.clone(), [0.0, 0.0, 0.0], 0, *page), None);
        }
        if let Some(outline_id) = out.build_outline() {
            if let Ok(d) = out.get_object_mut(outline_id).and_then(Object::as_dict_mut) {
                d.set("Type", Object::Name(b"Outlines".to_vec()));
            }
            catalog.set("Outlines", Object::Reference(outline_id));
            catalog.set("PageMode", Object::Name(b"UseOutlines".to_vec()));
        }
    }
    let catalog_id = out.add_object(Object::Dictionary(catalog));

    let mut info = Dictionary::new();
    info.set("Producer", Object::string_literal(format!("{} {}", crate::APP_NAME, crate::VERSION)));
    let info_id = out.add_object(Object::Dictionary(info));

    out.trailer = Dictionary::new();
    out.trailer.set("Root", Object::Reference(catalog_id));
    out.trailer.set("Info", Object::Reference(info_id));
    let id = Object::String(file_id(all_pages.len()).to_vec(), lopdf::StringFormat::Hexadecimal);
    out.trailer.set("ID", vec![id.clone(), id]);
    out.prune_objects();
    out.compress();
    Ok(out)
}

/// Identificativo del file (`/ID`), diverso a ogni unione.
fn file_id(pages: usize) -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut out = [0u8; 16];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(now);
        h.write_usize(pages);
        h.write_u32(std::process::id());
        h.write_usize(i);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    out
}

/// Rinumera tutti gli oggetti di `doc` a partire da `start` (generazione 0) e aggiorna ogni
/// riferimento. I riferimenti a oggetti che non esistono diventano `null`, come prevede il
/// formato. Restituisce il primo numero libero.
fn renumber(doc: &mut Document, start: u32) -> u32 {
    let mut map: HashMap<ObjectId, ObjectId> = HashMap::with_capacity(doc.objects.len());
    let mut next = start;
    for id in doc.objects.keys() {
        map.insert(*id, (next, 0));
        next += 1;
    }
    let old = std::mem::take(&mut doc.objects);
    for (id, mut obj) in old {
        remap(&mut obj, &map);
        doc.objects.insert(map[&id], obj);
    }
    for (_, v) in doc.trailer.iter_mut() {
        remap(v, &map);
    }
    doc.max_id = next.saturating_sub(1);
    next
}

fn remap(obj: &mut Object, map: &HashMap<ObjectId, ObjectId>) {
    match obj {
        Object::Reference(r) => match map.get(r) {
            Some(n) => *r = *n,
            None => *obj = Object::Null,
        },
        Object::Array(a) => a.iter_mut().for_each(|o| remap(o, map)),
        Object::Dictionary(d) => d.iter_mut().for_each(|(_, v)| remap(v, map)),
        Object::Stream(s) => s.dict.iter_mut().for_each(|(_, v)| remap(v, map)),
        _ => {}
    }
}

/// Copia sulla pagina gli attributi che eredita dai nodi dell'albero delle pagine.
fn inherit_attributes(doc: &mut Document, page: ObjectId) {
    let Ok(dict) = doc.get_dictionary(page) else { return };
    let mut missing: Vec<&[u8]> = INHERITABLE.iter().copied().filter(|k| !dict.has(k)).collect();
    if missing.is_empty() {
        return;
    }
    let mut found: Vec<(&[u8], Object)> = Vec::new();
    let mut parent = dict.get(b"Parent").and_then(Object::as_reference).ok();
    let mut visited = HashSet::new();
    while let Some(pid) = parent {
        if missing.is_empty() || !visited.insert(pid) {
            break;
        }
        let Ok(node) = doc.get_dictionary(pid) else { break };
        missing.retain(|k| match node.get(k) {
            Ok(v) => {
                found.push((k, v.clone()));
                false
            }
            Err(_) => true,
        });
        parent = node.get(b"Parent").and_then(Object::as_reference).ok();
    }
    if missing.contains(&&b"MediaBox"[..]) {
        // Obbligatoria: senza, i lettori usano il formato Lettera.
        found.push((b"MediaBox", Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()])));
    }
    if let Ok(d) = doc.get_object_mut(page).and_then(Object::as_dict_mut) {
        for (k, v) in found {
            d.set(k.to_vec(), v);
        }
    }
}

/// I nodi intermedi (`/Pages`) dell'albero delle pagine, che l'unione sostituisce.
fn page_tree_nodes(doc: &Document) -> HashSet<ObjectId> {
    let mut nodes = HashSet::new();
    let Ok(root) = doc.catalog().and_then(|c| c.get(b"Pages")).and_then(Object::as_reference) else {
        return nodes;
    };
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let Ok(d) = doc.get_dictionary(id) else { continue };
        let is_node = d.has(b"Kids") && !d.get(b"Type").and_then(Object::as_name).is_ok_and(|n| n == b"Page");
        if !is_node || !nodes.insert(id) {
            continue;
        }
        if let Ok(kids) = d.get(b"Kids").and_then(Object::as_array) {
            stack.extend(kids.iter().filter_map(|k| k.as_reference().ok()));
        }
    }
    nodes
}

/// Risolve un oggetto che può essere un riferimento.
fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    doc.dereference(obj).map(|(_, o)| o).unwrap_or(obj)
}

/// Campi dei moduli (AcroForm) di tutti i documenti uniti.
#[derive(Default)]
struct FormMerge {
    fields: Vec<Object>,
    base: Option<Dictionary>,
    need_appearances: bool,
}

impl FormMerge {
    fn collect(&mut self, doc: &Document, catalog: &Dictionary) {
        let Ok(acro) = catalog.get(b"AcroForm") else { return };
        let Ok(acro) = resolve(doc, acro).as_dict() else { return };
        if let Ok(fields) = acro.get(b"Fields") {
            if let Ok(arr) = resolve(doc, fields).as_array() {
                self.fields.extend(arr.iter().cloned());
            }
        }
        if acro.get(b"NeedAppearances").and_then(Object::as_bool).unwrap_or(false) {
            self.need_appearances = true;
        }
        if self.base.is_none() {
            let mut base = Dictionary::new();
            for key in [&b"DR"[..], b"DA", b"Q"] {
                if let Ok(v) = acro.get(key) {
                    base.set(key.to_vec(), v.clone());
                }
            }
            self.base = Some(base);
        }
    }

    fn build(self) -> Option<Dictionary> {
        if self.fields.is_empty() {
            return None;
        }
        let mut d = self.base.unwrap_or_default();
        d.set("Fields", Object::Array(self.fields));
        if self.need_appearances {
            d.set("NeedAppearances", Object::Boolean(true));
        }
        Some(d)
    }
}

/// Livelli (contenuto opzionale, `/OCProperties`) di tutti i documenti uniti.
#[derive(Default)]
struct LayerMerge {
    ocgs: Vec<Object>,
    on: Vec<Object>,
    off: Vec<Object>,
    order: Vec<Object>,
    locked: Vec<Object>,
    rb_groups: Vec<Object>,
}

impl LayerMerge {
    fn collect(&mut self, doc: &Document, catalog: &Dictionary) {
        let Ok(oc) = catalog.get(b"OCProperties") else { return };
        let Ok(oc) = resolve(doc, oc).as_dict() else { return };
        let arr = |d: &Dictionary, k: &[u8]| -> Vec<Object> {
            d.get(k).ok().and_then(|v| resolve(doc, v).as_array().ok().cloned()).unwrap_or_default()
        };
        self.ocgs.extend(arr(oc, b"OCGs"));
        if let Some(d) = oc.get(b"D").ok().and_then(|v| resolve(doc, v).as_dict().ok()) {
            self.on.extend(arr(d, b"ON"));
            self.off.extend(arr(d, b"OFF"));
            self.order.extend(arr(d, b"Order"));
            self.locked.extend(arr(d, b"Locked"));
            self.rb_groups.extend(arr(d, b"RBGroups"));
        }
    }

    fn build(self) -> Option<Dictionary> {
        if self.ocgs.is_empty() {
            return None;
        }
        let mut d = Dictionary::new();
        for (k, v) in [("ON", self.on), ("OFF", self.off), ("Order", self.order), ("Locked", self.locked), ("RBGroups", self.rb_groups)] {
            if !v.is_empty() {
                d.set(k, Object::Array(v));
            }
        }
        let mut oc = Dictionary::new();
        oc.set("OCGs", Object::Array(self.ocgs));
        oc.set("D", Object::Dictionary(d));
        Some(oc)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use lopdf::content::{Content, Operation};
    use lopdf::{dictionary, Stream};

    /// PDF di prova: una pagina per etichetta, ognuna col suo testo; `nested` mette le pagine
    /// sotto un nodo intermedio che porta MediaBox, risorse e rotazione (da ereditare).
    pub fn sample_pdf(labels: &[&str], nested: bool, rotate: i64) -> Document {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let parent = if nested { doc.new_object_id() } else { pages_id };
        let mut kids = vec![];
        for label in labels {
            let content = Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 24.into()]),
                    Operation::new("Td", vec![72.into(), 700.into()]),
                    Operation::new("Tj", vec![Object::string_literal(*label)]),
                    Operation::new("ET", vec![]),
                ],
            };
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
            let mut page = dictionary! {
                "Type" => "Page",
                "Parent" => parent,
                "Contents" => content_id,
            };
            if !nested {
                page.set("Resources", resources_id);
                page.set("MediaBox", vec![0.into(), 0.into(), 595.into(), 842.into()]);
            }
            kids.push(doc.add_object(page).into());
        }
        if nested {
            let n = kids.len() as i64;
            doc.objects.insert(
                parent,
                Object::Dictionary(dictionary! {
                    "Type" => "Pages",
                    "Parent" => pages_id,
                    "Kids" => kids,
                    "Count" => n,
                    "Resources" => resources_id,
                    "MediaBox" => vec![0.into(), 0.into(), 300.into(), 500.into()],
                    "Rotate" => rotate,
                }),
            );
            doc.objects.insert(
                pages_id,
                Object::Dictionary(dictionary! {
                    "Type" => "Pages",
                    "Kids" => vec![parent.into()],
                    "Count" => n,
                }),
            );
        } else {
            let n = kids.len() as i64;
            doc.objects.insert(
                pages_id,
                Object::Dictionary(dictionary! {
                    "Type" => "Pages",
                    "Kids" => kids,
                    "Count" => n,
                }),
            );
        }
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc
    }

    pub fn to_bytes(doc: &mut Document) -> Vec<u8> {
        let mut v = Vec::new();
        doc.save_to(&mut v).unwrap();
        v
    }

    fn page_texts(doc: &Document) -> Vec<String> {
        let n = doc.get_pages().len() as u32;
        (1..=n).map(|p| doc.extract_text(&[p]).unwrap_or_default().trim().to_string()).collect()
    }

    fn roundtrip(docs: Vec<(String, Document)>, bookmarks: bool) -> Document {
        let mut merged = merge_documents(docs, bookmarks).unwrap();
        let bytes = to_bytes(&mut merged);
        Document::load_mem(&bytes).unwrap()
    }

    #[test]
    fn unisce_nell_ordine_dato() {
        let a = sample_pdf(&["A1", "A2"], false, 0);
        let b = sample_pdf(&["B1"], false, 0);
        let c = sample_pdf(&["C1", "C2", "C3"], false, 0);
        let merged = roundtrip(vec![("a".into(), a), ("b".into(), b), ("c".into(), c)], true);
        assert_eq!(page_texts(&merged), vec!["A1", "A2", "B1", "C1", "C2", "C3"]);
    }

    #[test]
    fn ordine_delle_pagine_indipendente_dai_numeri_degli_oggetti() {
        // Pagine elencate in ordine inverso rispetto ai numeri degli oggetti.
        let mut a = sample_pdf(&["P1", "P2", "P3"], false, 0);
        let pages_id = a.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
        let kids = a.get_dictionary(pages_id).unwrap().get(b"Kids").unwrap().as_array().unwrap().clone();
        let reversed: Vec<Object> = kids.into_iter().rev().collect();
        a.get_object_mut(pages_id).unwrap().as_dict_mut().unwrap().set("Kids", reversed);
        let merged = roundtrip(vec![("a".into(), a), ("b".into(), sample_pdf(&["X"], false, 0))], false);
        assert_eq!(page_texts(&merged), vec!["P3", "P2", "P1", "X"]);
    }

    #[test]
    fn attributi_ereditati_copiati_sulle_pagine() {
        let a = sample_pdf(&["N1", "N2"], true, 90);
        let b = sample_pdf(&["F1"], false, 0);
        let merged = roundtrip(vec![("a".into(), a), ("b".into(), b)], false);
        let pages = merged.get_pages();
        let p1 = merged.get_dictionary(pages[&1]).unwrap();
        assert_eq!(p1.get(b"Rotate").unwrap().as_i64().unwrap(), 90);
        let mb: Vec<f32> = p1.get(b"MediaBox").unwrap().as_array().unwrap().iter().map(|o| o.as_float().unwrap()).collect();
        assert_eq!(mb, vec![0.0, 0.0, 300.0, 500.0]);
        assert!(p1.has(b"Resources"));
        let p3 = merged.get_dictionary(pages[&3]).unwrap();
        assert!(!p3.has(b"Rotate"));
        assert_eq!(page_texts(&merged), vec!["N1", "N2", "F1"]);
        // un solo nodo /Pages nel risultato
        let root = merged.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
        assert_eq!(merged.get_dictionary(root).unwrap().get(b"Count").unwrap().as_i64().unwrap(), 3);
        assert_eq!(p1.get(b"Parent").unwrap().as_reference().unwrap(), root);
    }

    #[test]
    fn segnalibri_per_file() {
        let a = sample_pdf(&["A1", "A2"], false, 0);
        let b = sample_pdf(&["B1"], false, 0);
        let merged = roundtrip(vec![("Primo".into(), a), ("Secondo è".into(), b)], true);
        let outlines = merged.catalog().unwrap().get(b"Outlines").unwrap().as_reference().unwrap();
        let o = merged.get_dictionary(outlines).unwrap();
        assert_eq!(o.get(b"Count").unwrap().as_i64().unwrap(), 2);
        let first = merged.get_dictionary(o.get(b"First").unwrap().as_reference().unwrap()).unwrap();
        assert_eq!(first.get(b"Title").unwrap().as_str().unwrap(), b"Primo");
        // Senza segnalibri: nessun /Outlines
        let merged = roundtrip(vec![("a".into(), sample_pdf(&["A"], false, 0)), ("b".into(), sample_pdf(&["B"], false, 0))], false);
        assert!(merged.catalog().unwrap().get(b"Outlines").is_err());
    }

    #[test]
    fn stesso_file_due_volte() {
        let mut a = sample_pdf(&["S1", "S2"], false, 0);
        let bytes = to_bytes(&mut a);
        let d1 = Document::load_mem(&bytes).unwrap();
        let d2 = Document::load_mem(&bytes).unwrap();
        let merged = roundtrip(vec![("x".into(), d1), ("x".into(), d2)], true);
        assert_eq!(page_texts(&merged), vec!["S1", "S2", "S1", "S2"]);
    }

    #[test]
    fn documento_senza_pagine() {
        let mut empty = sample_pdf(&[], false, 0);
        let bytes = to_bytes(&mut empty);
        let e = Document::load_mem(&bytes).unwrap();
        assert!(merge_documents(vec![("e".into(), e)], true).is_err());
        let e = Document::load_mem(&bytes).unwrap();
        let merged = roundtrip(vec![("e".into(), e), ("a".into(), sample_pdf(&["A"], false, 0))], true);
        assert_eq!(page_texts(&merged), vec!["A"]);
    }

    #[test]
    fn campi_dei_moduli_conservati() {
        let mut a = sample_pdf(&["A1"], false, 0);
        let page = a.get_pages()[&1];
        let field = a.add_object(dictionary! {
            "FT" => "Tx",
            "T" => Object::string_literal("nome"),
            "Type" => "Annot",
            "Subtype" => "Widget",
            "Rect" => vec![10.into(), 10.into(), 100.into(), 30.into()],
            "P" => page,
        });
        a.get_object_mut(page).unwrap().as_dict_mut().unwrap().set("Annots", vec![field.into()]);
        let root = a.trailer.get(b"Root").unwrap().as_reference().unwrap();
        a.get_object_mut(root).unwrap().as_dict_mut().unwrap().set("AcroForm", dictionary! { "Fields" => vec![field.into()] });
        let merged = roundtrip(vec![("a".into(), a), ("b".into(), sample_pdf(&["B"], false, 0))], false);
        let acro = merged.catalog().unwrap().get(b"AcroForm").unwrap().as_reference().unwrap();
        let fields = merged.get_dictionary(acro).unwrap().get(b"Fields").unwrap().as_array().unwrap();
        assert_eq!(fields.len(), 1);
        let f = merged.get_dictionary(fields[0].as_reference().unwrap()).unwrap();
        assert_eq!(f.get(b"T").unwrap().as_str().unwrap(), b"nome");
    }

    #[test]
    fn pdf_cifrati() {
        use lopdf::{EncryptionState, EncryptionVersion, Permissions};
        let encrypt = |owner: &str, user: &str| {
            let mut d = sample_pdf(&["SEGRETO"], false, 0);
            let bytes = to_bytes(&mut d);
            let mut d = Document::load_mem(&bytes).unwrap();
            let id = Object::String(b"0123456789abcdef".to_vec(), lopdf::StringFormat::Hexadecimal);
            d.trailer.set("ID", vec![id.clone(), id]);
            let version = EncryptionVersion::V2 {
                document: &d,
                owner_password: owner,
                user_password: user,
                key_length: 128,
                permissions: Permissions::all(),
            };
            let state = EncryptionState::try_from(version).unwrap();
            d.encrypt(&state).unwrap();
            to_bytes(&mut d)
        };
        // Solo password del proprietario: si apre e si unisce senza chiedere nulla.
        let owner_only = encrypt("padrone", "");
        let (doc, _) = load_document(&owner_only, &[]).unwrap();
        let merged = roundtrip(vec![("a".into(), doc), ("b".into(), sample_pdf(&["B"], false, 0))], false);
        assert_eq!(page_texts(&merged), vec!["SEGRETO", "B"]);
        // Password di apertura: serve quella giusta.
        let locked = encrypt("padrone", "1234");
        assert!(load_document(&locked, &[]).is_err());
        assert!(load_document(&locked, &["no".into()]).is_err());
        let (doc, _) = load_document(&locked, &["no".into(), "1234".into()]).unwrap();
        let merged = roundtrip(vec![("a".into(), doc)], false);
        assert_eq!(page_texts(&merged), vec!["SEGRETO"]);
    }

    #[test]
    fn unione_su_disco_e_uscita_uguale_a_un_ingresso() {
        let dir = std::env::temp_dir().join(format!("mypdf-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.pdf");
        let b = dir.join("b.pdf");
        std::fs::write(&a, to_bytes(&mut sample_pdf(&["A1", "A2"], false, 0))).unwrap();
        std::fs::write(&b, to_bytes(&mut sample_pdf(&["B1"], true, 0))).unwrap();
        let out = dir.join("unito.pdf");
        let r = merge_files(&[a.clone(), b.clone()], &out, &MergeOptions::default()).unwrap();
        assert_eq!(r, MergeReport { files: 2, pages: 3, repaired: vec![] });
        let merged = Document::load(&out).unwrap();
        assert_eq!(page_texts(&merged), vec!["A1", "A2", "B1"]);
        // L'uscita sovrascrive uno degli ingressi: deve funzionare.
        merge_files(&[a.clone(), b.clone()], &a, &MergeOptions::default()).unwrap();
        assert_eq!(page_texts(&Document::load(&a).unwrap()), vec!["A1", "A2", "B1"]);
        // Nessun file temporaneo rimasto.
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(leftovers.is_empty());
        // File che non è un PDF: errore con il nome del file.
        let bad = dir.join("rotto.pdf");
        std::fs::write(&bad, b"non sono un pdf").unwrap();
        let e = merge_files(&[a.clone(), bad.clone()], &out, &MergeOptions::default()).unwrap_err();
        assert_eq!(e.file.as_deref(), Some(bad.as_path()));
        assert!(e.message.contains("non è un file PDF"), "{}", e.message);
        // PDF troncato: si uniscono le pagine leggibili e lo si segnala.
        let full = to_bytes(&mut sample_pdf(&["T1", "T2", "T3", "T4"], false, 0));
        let cut = dir.join("troncato.pdf");
        std::fs::write(&cut, &full[..full.len() * 3 / 4]).unwrap();
        let r = merge_files(&[b.clone(), cut.clone()], &out, &MergeOptions::default()).unwrap();
        assert_eq!(r.repaired, vec![cut.clone()]);
        let texts = page_texts(&Document::load(&out).unwrap());
        assert_eq!(texts[0], "B1");
        assert!(texts.len() >= 2 && texts[1] == "T1", "{texts:?}");
        // PDF senza nessuna pagina leggibile: errore chiaro
        std::fs::write(&bad, b"%PDF-1.4\n%solo intestazione").unwrap();
        let e = merge_files(&[a.clone(), bad.clone()], &out, &MergeOptions::default()).unwrap_err();
        assert!(e.message.contains("broken PDF format"), "{}", e.message);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
