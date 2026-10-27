//! Liaison avec la bibliothèque espeak-ng du système.
//!
//! Sous macOS elle vient de Homebrew (`brew install espeak-ng`), sous Linux du
//! paquet `libespeak-ng-dev`. `ESPEAK_NG_LIB_DIR` force un autre emplacement.

fn main() {
    println!("cargo:rerun-if-env-changed=ESPEAK_NG_LIB_DIR");
    if let Ok(dir) = std::env::var("ESPEAK_NG_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    } else if cfg!(target_os = "macos") {
        for dir in ["/opt/homebrew/lib", "/usr/local/lib"] {
            if std::path::Path::new(dir)
                .join("libespeak-ng.dylib")
                .exists()
            {
                println!("cargo:rustc-link-search=native={dir}");
            }
        }
    }
    println!("cargo:rustc-link-lib=dylib=espeak-ng");
}
