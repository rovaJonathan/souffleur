//! Outils de développement de Souffleur.
//!
//!     cargo xtask icons       régénère les icônes depuis assets/logo.svg
//!     cargo xtask macos-app   installe Souffleur.app dans ~/Applications

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

type Result<T = ()> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let result = match env::args().nth(1).as_deref() {
        Some("icons") => icons(),
        Some("macos-app") => macos_app(),
        _ => Err("usage : cargo xtask <icons | macos-app>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn project_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask est dans le dépôt")
        .to_path_buf()
}

fn run(command: &mut Command) -> Result {
    let status = command
        .status()
        .map_err(|e| format!("{:?} : {e}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} a échoué ({status})", command.get_program()))
    }
}

fn io<T>(result: std::io::Result<T>, what: impl std::fmt::Display) -> Result<T> {
    result.map_err(|e| format!("{what} : {e}"))
}

// --------------------------------------------------------------------------- //
// Icônes
// --------------------------------------------------------------------------- //

const PNG_SIZES: [u32; 7] = [16, 32, 64, 128, 256, 512, 1024];
const ICO_SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];
/// Nom d'entrée iconset -> taille en pixels (les « @2x » sont des variantes Retina).
const ICNS_ENTRIES: [(&str, u32); 10] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

/// Génère, à partir de `assets/logo.svg` :
///
/// * `icon-<taille>.png` (16 à 1024 px) — l'icône de fenêtre et la source des
///   autres formats ;
/// * `icon.ico` (Windows) — écrit ici même : un `.ico` peut embarquer des PNG
///   tels quels ;
/// * `icon.icns` (macOS) — via `iconutil`, donc seulement sous macOS.
///
/// Nécessite `rsvg-convert` (`brew install librsvg` / `apt install
/// librsvg2-bin`). Les fichiers générés sont versionnés : l'application se
/// compile sans cet outil.
fn icons() -> Result {
    let assets = project_dir().join("assets");
    let svg = assets.join("logo.svg");
    let render = |size: u32, target: &Path| {
        let size = size.to_string();
        run(Command::new("rsvg-convert")
            .args(["-w", &size, "-h", &size])
            .arg(&svg)
            .arg("-o")
            .arg(target))
        .map_err(|e| {
            format!("{e} (rsvg-convert : brew install librsvg / apt install librsvg2-bin)")
        })
    };

    for size in PNG_SIZES {
        render(size, &assets.join(format!("icon-{size}.png")))?;
        println!("assets/icon-{size}.png");
    }

    let tmp = env::temp_dir().join(format!("souffleur-icons-{}", std::process::id()));
    io(fs::create_dir_all(&tmp), tmp.display())?;
    let result = (|| {
        let mut pngs = Vec::new();
        for size in ICO_SIZES {
            let path = tmp.join(format!("{size}.png"));
            render(size, &path)?;
            pngs.push((size, io(fs::read(&path), path.display())?));
        }
        io(fs::write(assets.join("icon.ico"), ico(&pngs)), "icon.ico")?;
        println!("assets/icon.ico");

        if cfg!(target_os = "macos") {
            let iconset = tmp.join("icon.iconset");
            io(fs::create_dir_all(&iconset), iconset.display())?;
            for (name, size) in ICNS_ENTRIES {
                render(size, &iconset.join(name))?;
            }
            run(Command::new("iconutil")
                .args(["-c", "icns"])
                .arg(&iconset)
                .arg("-o")
                .arg(assets.join("icon.icns")))?;
            println!("assets/icon.icns");
        } else {
            println!("icon.icns ignoré : iconutil n'est disponible que sous macOS");
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

/// Assemble un .ico contenant des images PNG (supporté depuis Windows Vista).
fn ico(pngs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let count = pngs.len() as u16;
    let mut out = Vec::new();
    out.extend([0u16, 1, count].iter().flat_map(|v| v.to_le_bytes()));
    let mut offset = 6 + 16 * pngs.len() as u32;
    for (size, data) in pngs {
        let dim = if *size >= 256 { 0 } else { *size as u8 }; // 0 signifie 256
        out.extend([dim, dim, 0, 0]);
        out.extend(1u16.to_le_bytes()); // plans
        out.extend(32u16.to_le_bytes()); // bits par pixel
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in pngs {
        out.extend(data);
    }
    out
}

// --------------------------------------------------------------------------- //
// Application macOS
// --------------------------------------------------------------------------- //

const BUNDLE_NAME: &str = "Souffleur.app";
const BUNDLE_ID: &str = "app.souffleur.gui";

/// Compile en mode optimisé et installe `~/Applications/Souffleur.app`.
///
/// Le .app contient le binaire lui-même : il ne dépend plus du dépôt. Il reste
/// lié à l'espeak-ng de Homebrew (`brew install espeak-ng`) et embarque donc,
/// une fois distribué, du code GPL-3.0 (voir README).
fn macos_app() -> Result {
    if !cfg!(target_os = "macos") {
        return Err("macos-app n'a de sens que sous macOS".into());
    }
    let project = project_dir();
    run(
        Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(&project)
            .args(["build", "--release", "--package", "souffleur"]),
    )?;

    let bundle = project.join("dist").join(BUNDLE_NAME);
    if bundle.exists() {
        io(fs::remove_dir_all(&bundle), bundle.display())?;
    }
    let contents = bundle.join("Contents");
    let (macos, resources) = (contents.join("MacOS"), contents.join("Resources"));
    io(fs::create_dir_all(&macos), macos.display())?;
    io(fs::create_dir_all(&resources), resources.display())?;

    let icon = project.join("assets").join("icon.icns");
    let has_icon = icon.is_file();
    if has_icon {
        io(
            fs::copy(&icon, resources.join("AppIcon.icns")),
            icon.display(),
        )?;
    }
    io(
        fs::write(contents.join("Info.plist"), info_plist(has_icon)),
        "Info.plist",
    )?;
    io(fs::write(contents.join("PkgInfo"), "APPLSufl"), "PkgInfo")?;
    let binary = project.join("target").join("release").join("souffleur");
    io(fs::copy(&binary, macos.join("Souffleur")), binary.display())?;

    let home = env::var_os("HOME").ok_or("HOME non défini")?;
    let install = PathBuf::from(home).join("Applications").join(BUNDLE_NAME);
    if install.exists() {
        io(fs::remove_dir_all(&install), install.display())?;
    }
    io(
        fs::create_dir_all(install.parent().unwrap()),
        "~/Applications",
    )?;
    // `ditto` copie le bundle tel quel (attributs étendus compris).
    run(Command::new("ditto").arg(&bundle).arg(&install))?;
    println!("{}", install.display());
    Ok(())
}

fn info_plist(has_icon: bool) -> String {
    let icon_entry = if has_icon {
        "    <key>CFBundleIconFile</key>\n    <string>AppIcon</string>\n"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>fr</string>
    <key>CFBundleDisplayName</key>
    <string>Souffleur</string>
    <key>CFBundleExecutable</key>
    <string>Souffleur</string>
    <key>CFBundleIdentifier</key>
    <string>{BUNDLE_ID}</string>
{icon_entry}    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Souffleur</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>MIT</string>
</dict>
</plist>
"#
    )
}
