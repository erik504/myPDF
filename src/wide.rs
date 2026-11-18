//! Conversioni tra stringhe Rust e UTF-16 di Windows.

use windows::core::PCWSTR;

/// Stringa UTF-16 terminata da zero, da passare alle API Win32 finché è viva.
pub struct WStr(Vec<u16>);

impl WStr {
    pub fn new(s: &str) -> Self {
        WStr(s.encode_utf16().chain(std::iter::once(0)).collect())
    }
    pub fn from_os(s: &std::ffi::OsStr) -> Self {
        use std::os::windows::ffi::OsStrExt;
        WStr(s.encode_wide().chain(std::iter::once(0)).collect())
    }
    pub fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
    pub fn as_slice_with_nul(&self) -> &[u16] {
        &self.0
    }
}

/// Testo da un buffer UTF-16 che può finire con degli zeri.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Percorso da un buffer UTF-16 (anche con caratteri non validi in UTF-16).
pub fn path_from_wide(buf: &[u16]) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    std::ffi::OsString::from_wide(&buf[..end]).into()
}
