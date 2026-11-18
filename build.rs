use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=res/myPDF.ico");
    println!("cargo:rerun-if-changed=res/app.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // Le DLL di sistema si caricano solo da System32, mai dalla cartella dell'exe
        // (es. Download con una DLL piazzata accanto).
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
    // Icona, manifest (stili moderni, DPI per monitor, Windows 10/11) e informazioni di
    // versione, con la versione presa da Cargo.toml.
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let comma = parts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let dotted = parts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(".");
    let esc = |p: PathBuf| p.display().to_string().replace('\\', "\\\\");
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {comma}
PRODUCTVERSION {comma}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "041004B0"
    BEGIN
      VALUE "CompanyName", "myPDF"
      VALUE "FileDescription", "myPDF - visualizzatore e unione di PDF"
      VALUE "ProductName", "myPDF"
      VALUE "FileVersion", "{dotted}"
      VALUE "ProductVersion", "{dotted}"
      VALUE "OriginalFilename", "myPDF.exe"
      VALUE "InternalName", "myPDF"
      VALUE "LegalCopyright", "Licenza MIT"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x410, 1200
  END
END
"#,
        icon = esc(root.join("res").join("myPDF.ico")),
        manifest = esc(root.join("res").join("app.manifest")),
    );
    let rc_path = out.join("app.rc");
    std::fs::write(&rc_path, rc).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_required().unwrap();
}
