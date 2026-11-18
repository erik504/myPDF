//! Elenchi di file: caratteri jolly (`*`, `?`), cartelle e ordinamento "naturale"
//! (`pag2.pdf` prima di `pag10.pdf`), come in Esplora file.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// Vero se il nome ha estensione `.pdf` (maiuscole o minuscole).
pub fn is_pdf(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Confronto naturale tra nomi: le cifre si confrontano come numeri, il resto senza
/// distinguere maiuscole e minuscole.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let si = i;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let na: String = a[si..i].iter().collect::<String>().trim_start_matches('0').to_string();
            let nb: String = b[sj..j].iter().collect::<String>().trim_start_matches('0').to_string();
            let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            let ca = a[i].to_lowercase().next().unwrap_or(a[i]);
            let cb = b[j].to_lowercase().next().unwrap_or(b[j]);
            if ca != cb {
                return ca.cmp(&cb);
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

/// Ordina percorsi per nome del file, in ordine naturale.
pub fn sort_paths(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| {
        let na = a.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let nb = b.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        natural_cmp(&na, &nb)
    });
}

/// Confronto con caratteri jolly `*` e `?`, senza distinguere maiuscole e minuscole.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Trasforma un argomento della riga di comando in uno o più file PDF:
/// - un file → se stesso;
/// - una cartella → i suoi `.pdf` (non nelle sottocartelle), in ordine naturale;
/// - un nome con `*` o `?` → i file che corrispondono, in ordine naturale.
pub fn expand_arg(arg: &str) -> Result<Vec<PathBuf>, String> {
    let path = PathBuf::from(arg);
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if name.contains('*') || name.contains('?') {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        if dir.to_string_lossy().contains(['*', '?']) {
            return Err(format!("{arg}: i caratteri jolly sono ammessi solo nel nome del file"));
        }
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| wildcard_match(&name, &e.file_name().to_string_lossy()))
            .map(|e| if path.parent().is_some_and(|p| p.as_os_str().is_empty()) { PathBuf::from(e.file_name()) } else { e.path() })
            .collect();
        if found.is_empty() {
            return Err(format!("{arg}: nessun file corrisponde"));
        }
        sort_paths(&mut found);
        return Ok(found);
    }
    if path.is_dir() {
        let entries = std::fs::read_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut found: Vec<PathBuf> = entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_file())).map(|e| e.path()).filter(|p| is_pdf(p)).collect();
        if found.is_empty() {
            return Err(format!("{}: la cartella non contiene PDF", path.display()));
        }
        sort_paths(&mut found);
        return Ok(found);
    }
    if !path.is_file() {
        return Err(format!("{arg}: file non trovato"));
    }
    Ok(vec![path])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordine_naturale() {
        let mut v = vec!["pag10.pdf", "Pag2.pdf", "pag1.pdf", "pag02b.pdf", "a.pdf"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["a.pdf", "pag1.pdf", "Pag2.pdf", "pag02b.pdf", "pag10.pdf"]);
        assert_eq!(natural_cmp("x", "x"), Ordering::Equal);
        assert_eq!(natural_cmp("x", "x1"), Ordering::Less);
    }

    #[test]
    fn caratteri_jolly() {
        assert!(wildcard_match("*.pdf", "Fattura 3.PDF"));
        assert!(wildcard_match("f?ttura*.pdf", "fattura_2024.pdf"));
        assert!(!wildcard_match("*.pdf", "nota.txt"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("a*b*c", "a-x-b-y-c"));
        assert!(!wildcard_match("a*b*c", "a-x-c-y-b"));
    }

    #[test]
    fn espansione_argomenti() {
        let dir = std::env::temp_dir().join(format!("mypdf-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["s10.pdf", "s2.pdf", "s1.PDF", "nota.txt"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let names = |v: Vec<PathBuf>| v.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();
        let got = expand_arg(&dir.join("s*.pdf").to_string_lossy()).unwrap();
        assert_eq!(names(got), vec!["s1.PDF", "s2.pdf", "s10.pdf"]);
        let got = expand_arg(&dir.to_string_lossy()).unwrap();
        assert_eq!(names(got), vec!["s1.PDF", "s2.pdf", "s10.pdf"]);
        assert!(expand_arg(&dir.join("zz*.pdf").to_string_lossy()).is_err());
        assert!(expand_arg(&dir.join("manca.pdf").to_string_lossy()).is_err());
        let one = dir.join("s2.pdf");
        assert_eq!(expand_arg(&one.to_string_lossy()).unwrap(), vec![one.clone()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
