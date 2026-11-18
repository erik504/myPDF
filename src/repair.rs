//! Recupero delle pagine leggibili da PDF danneggiati o troncati.
//!
//! Il file viene tagliato dopo l'ultimo oggetto completo; in coda si aggiungono un catalogo e
//! un albero delle pagine nuovi e un trailer, poi lopdf ricostruisce la tabella degli oggetti
//! leggendo il file da cima a fondo. Le pagine sopravvissute (nell'ordine dell'albero
//! originale, se c'è ancora, altrimenti in quello del file) diventano un PDF nuovo e pulito.

use lopdf::{Dictionary, Document, LoadOptions, Object, ObjectId};
use std::collections::HashSet;

/// Numero massimo di oggetti che lopdf accetta ricostruendo un file.
const MAX_OBJECTS: u32 = 999_000;

/// Vero se all'inizio del file c'è l'intestazione di un PDF (`%PDF-`).
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    find(&bytes[..bytes.len().min(1024)], b"%PDF-").is_some()
}

/// Vero se il file finisce come un PDF completo (`%%EOF` negli ultimi byte).
pub fn has_eof_marker(bytes: &[u8]) -> bool {
    let tail = &bytes[bytes.len().saturating_sub(1024)..];
    find(tail, b"%%EOF").is_some()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// Pagine recuperate da un PDF danneggiato.
pub struct Salvaged {
    /// Un PDF nuovo con le sole pagine leggibili.
    pub pdf: Vec<u8>,
    pub pages: usize,
}

/// Recupera le pagine leggibili; `None` se non ce n'è nessuna (o il file è cifrato).
pub fn salvage(bytes: &[u8]) -> Option<Salvaged> {
    // lopdf su un file rovinato potrebbe anche andare in panico: non deve chiudere il programma.
    std::panic::catch_unwind(|| salvage_doc(bytes)).ok().flatten().and_then(|mut doc| {
        let pages = doc.get_pages().len();
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).ok()?;
        Some(Salvaged { pdf, pages })
    })
}

/// Come [`salvage`], ma restituisce il documento lopdf (per l'unione).
pub fn salvage_doc(bytes: &[u8]) -> Option<Document> {
    if !looks_like_pdf(bytes) {
        return None;
    }
    let patched = patch_tail(bytes)?;
    let opts = LoadOptions { force_reconstruct: true, ..Default::default() };
    let mut doc = Document::load_mem_with_options(&patched, opts).ok()?;
    if doc.is_encrypted() {
        return None;
    }
    let pages = collect_pages(&doc);
    if pages.is_empty() {
        return None;
    }
    let count = pages.len() as i64;
    let kids: Vec<Object> = pages.into_iter().map(Object::Reference).collect();
    let mut node = Dictionary::new();
    node.set("Type", Object::Name(b"Pages".to_vec()));
    node.set("Kids", Object::Array(kids));
    node.set("Count", Object::Integer(count));
    let node_id = doc.add_object(Object::Dictionary(node));
    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(node_id));
    let catalog_id = doc.add_object(Object::Dictionary(catalog));
    doc.trailer = Dictionary::new();
    doc.trailer.set("Root", Object::Reference(catalog_id));
    // Ripulito come in un'unione: attributi ereditati copiati sulle pagine, oggetti inutili via.
    crate::merge::merge_documents(vec![(String::new(), doc)], false).ok()
}

/// Taglia il file dopo l'ultimo `endobj` e aggiunge catalogo, albero delle pagine e trailer.
fn patch_tail(bytes: &[u8]) -> Option<Vec<u8>> {
    let end = rfind(bytes, b"endobj")? + b"endobj".len();
    let max = max_object_number(&bytes[..end]);
    let (catalog, pages) = (max + 1, max + 2);
    if pages >= MAX_OBJECTS {
        return None;
    }
    let mut v = bytes[..end].to_vec();
    v.extend_from_slice(
        format!(
            "\n{catalog} 0 obj\n<< /Type /Catalog /Pages {pages} 0 R >>\nendobj\n\
             {pages} 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n\
             trailer\n<< /Size {} /Root {catalog} 0 R >>\nstartxref\n0\n%%EOF\n",
            pages + 1
        )
        .as_bytes(),
    );
    Some(v)
}

/// Il numero più alto tra le intestazioni `N G obj` del file.
fn max_object_number(bytes: &[u8]) -> u32 {
    let ws = |c: u8| matches!(c, b' ' | b'\r' | b'\n' | b'\t' | b'\x0c' | 0);
    let mut max = 0u32;
    let mut i = 0;
    while let Some(off) = find(&bytes[i..], b"obj") {
        let pos = i + off;
        i = pos + 3;
        if pos == 0 || !ws(bytes[pos - 1]) {
            continue; // "endobj" o altro
        }
        // all'indietro: spazi, generazione, spazi, numero
        let mut j = pos;
        while j > 0 && ws(bytes[j - 1]) {
            j -= 1;
        }
        let gen_end = j;
        while j > 0 && bytes[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j == gen_end {
            continue;
        }
        let mut k = j;
        while k > 0 && ws(bytes[k - 1]) {
            k -= 1;
        }
        if k == j {
            continue;
        }
        let num_end = k;
        while k > 0 && bytes[k - 1].is_ascii_digit() {
            k -= 1;
        }
        if k == num_end || num_end - k > 7 {
            continue;
        }
        if let Ok(n) = std::str::from_utf8(&bytes[k..num_end]).unwrap_or("x").parse::<u32>() {
            if n < MAX_OBJECTS {
                max = max.max(n);
            }
        }
    }
    max
}

fn is_type(obj: &Object, t: &[u8]) -> bool {
    obj.as_dict().ok().and_then(|d| d.get(b"Type").ok()).and_then(|o| o.as_name().ok()).is_some_and(|n| n == t)
}

/// Le pagine rimaste: prima quelle raggiungibili dagli alberi delle pagine originali (in
/// ordine), poi quelle sparse, nell'ordine in cui compaiono nel file.
fn collect_pages(doc: &Document) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    // radici: nodi /Pages senza un genitore valido
    let roots: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(_, o)| is_type(o, b"Pages"))
        .filter(|(_, o)| {
            let parent = o.as_dict().ok().and_then(|d| d.get(b"Parent").ok()).and_then(|p| p.as_reference().ok());
            !parent.is_some_and(|p| doc.objects.get(&p).is_some_and(|po| is_type(po, b"Pages")))
        })
        .map(|(id, _)| *id)
        .collect();
    let mut visited = HashSet::new();
    for root in roots {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !visited.insert(id) || visited.len() > 1_000_000 {
                continue;
            }
            let Some(obj) = doc.objects.get(&id) else { continue };
            if is_type(obj, b"Page") {
                if seen.insert(id) {
                    out.push(id);
                }
            } else if is_type(obj, b"Pages") {
                if let Some(kids) = obj.as_dict().ok().and_then(|d| d.get(b"Kids").ok()).and_then(|k| k.as_array().ok()) {
                    // al contrario sulla pila, così si visitano nell'ordine giusto
                    stack.extend(kids.iter().rev().filter_map(|k| k.as_reference().ok()));
                }
            }
        }
    }
    for (id, obj) in &doc.objects {
        if is_type(obj, b"Page") && seen.insert(*id) {
            out.push(*id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::tests::{sample_pdf, to_bytes};

    fn texts(pdf: &[u8]) -> Vec<String> {
        let doc = Document::load_mem(pdf).unwrap();
        let n = doc.get_pages().len() as u32;
        (1..=n).map(|p| doc.extract_text(&[p]).unwrap_or_default().trim().to_string()).collect()
    }

    #[test]
    fn riconosce_i_pdf() {
        assert!(looks_like_pdf(b"%PDF-1.7\n..."));
        assert!(looks_like_pdf(b"\xEF\xBB\xBF  %PDF-1.4")); // spazzatura prima dell'intestazione
        assert!(!looks_like_pdf(b"\x89PNG\r\n\x1a\n"));
        assert!(!looks_like_pdf(b""));
        assert!(has_eof_marker(b"%PDF-1.4 ... %%EOF\n"));
        assert!(!has_eof_marker(b"%PDF-1.4 ... 12 0 obj"));
    }

    #[test]
    fn numero_massimo_degli_oggetti() {
        assert_eq!(max_object_number(b"1 0 obj\n<<>>\nendobj\n12 0 obj\n<<>>\nendobj\n7 0 obj"), 12);
        assert_eq!(max_object_number(b"endobj endobj"), 0);
    }

    #[test]
    fn recupera_le_pagine_di_un_file_troncato() {
        let labels: Vec<String> = (1..=10).map(|i| format!("PAG{i}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let full = to_bytes(&mut sample_pdf(&refs, false, 0));
        // tagliato a varie altezze: si recupera un inizio del documento, nell'ordine giusto
        for pct in [35, 60, 85, 99] {
            let cut = &full[..full.len() * pct / 100];
            let s = salvage(cut).unwrap_or_else(|| panic!("niente recuperato al {pct}%"));
            let got = texts(&s.pdf);
            assert_eq!(got.len(), s.pages);
            assert!(!got.is_empty(), "{pct}%");
            assert_eq!(got, labels[..got.len()].to_vec(), "{pct}%");
        }
        // file intero: tutte le pagine
        assert_eq!(salvage(&full).unwrap().pages, 10);
    }

    #[test]
    fn pagine_sotto_un_nodo_intermedio() {
        let full = to_bytes(&mut sample_pdf(&["N1", "N2", "N3"], true, 90));
        let s = salvage(&full[..full.len() - 40]).unwrap();
        let doc = Document::load_mem(&s.pdf).unwrap();
        let first = doc.get_dictionary(doc.get_pages()[&1]).unwrap();
        // attributi ereditati dal nodo intermedio ancora presenti
        assert_eq!(first.get(b"Rotate").unwrap().as_i64().unwrap(), 90);
        assert_eq!(texts(&s.pdf), vec!["N1", "N2", "N3"]);
    }

    #[test]
    fn niente_da_recuperare() {
        assert!(salvage(b"").is_none());
        assert!(salvage(b"testo qualsiasi").is_none());
        assert!(salvage(b"%PDF-1.4\n%garbage senza oggetti").is_none());
        assert!(salvage(b"%PDF-1.4\n1 0 obj\n<< /Type /Font >>\nendobj\n").is_none());
        // dati casuali dopo un'intestazione PDF: nessun panico
        let mut noise = b"%PDF-1.7\n".to_vec();
        let mut x = 12345u32;
        for _ in 0..20000 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            noise.push((x >> 16) as u8);
        }
        noise.extend_from_slice(b"\n3 0 obj\n<< /Type /Page /Kids [ 4 0 R >>\nendobj\n");
        let _ = salvage(&noise);
    }
}
