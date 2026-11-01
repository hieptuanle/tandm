//! `tandm app install|uninstall`: make the GUI launchable like any other app.
//! macOS: `~/Applications/tandm.app`, whose executable is a symlink to the installed tandm
//! binary, so updating the binary updates the app. Linux: a `.desktop` entry plus an icon.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::paths;
use crate::util;

/// 1024×1024, rendered from assets/tandm.svg.
pub const ICON_PNG: &[u8] = include_bytes!("../../../assets/tandm.png");
pub const ICON_SVG: &[u8] = include_bytes!("../../../assets/tandm.svg");
pub const BUNDLE_ID: &str = "local.tandm.app";

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".local").join("share"))
}

pub fn macos_bundle() -> PathBuf {
    paths::home_dir().join("Applications").join("tandm.app")
}

fn desktop_file() -> PathBuf {
    data_home().join("applications").join("tandm.desktop")
}

fn linux_icon() -> PathBuf {
    // hicolor has no 1024x1024 dir; the SVG serves every size.
    data_home().join("icons/hicolor/scalable/apps/tandm.svg")
}

/// True when this process was started as the executable of an `.app` bundle (Finder, Dock,
/// Spotlight, `open`). LaunchServices resolves the bundle's symlink before exec, so argv[0] is the
/// real binary; what it does set is `__CFBundleIdentifier` to the bundle's id.
pub fn launched_from_bundle() -> bool {
    std::env::var("__CFBundleIdentifier").is_ok_and(|v| v == BUNDLE_ID)
        || std::env::args_os()
            .next()
            .is_some_and(|a| a.to_string_lossy().contains(".app/Contents/MacOS/"))
}

pub fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
  <key>CFBundleName</key><string>tandm</string>
  <key>CFBundleDisplayName</key><string>tandm</string>
  <key>CFBundleExecutable</key><string>tandm</string>
  <key>CFBundleIconFile</key><string>tandm</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSEnvironment</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
"#
    )
}

pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=tandm\n\
         Comment=Git repos in step across machines\n\
         Exec={} gui\n\
         Icon=tandm\n\
         Terminal=false\n\
         Categories=Development;\n\
         StartupWMClass=tandm\n",
        // Desktop-entry Exec quoting: wrap in quotes when there is a space.
        if exe.to_string_lossy().contains(' ') {
            format!("\"{}\"", exe.display())
        } else {
            exe.display().to_string()
        }
    )
}

/// Build `tandm.icns` from the embedded PNG with the stock `sips` + `iconutil`.
fn make_icns(out: &Path) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("tandm-icon-{}", std::process::id()));
    let set = tmp.join("tandm.iconset");
    fs::create_dir_all(&set)?;
    let src = tmp.join("src.png");
    fs::write(&src, ICON_PNG)?;
    let result = (|| -> Result<()> {
        for size in [16u32, 32, 128, 256, 512] {
            for (scale, suffix) in [(1, ""), (2, "@2x")] {
                let px = (size * scale).to_string();
                let dst = set.join(format!("icon_{size}x{size}{suffix}.png"));
                let o = Command::new("sips")
                    .args(["-z", &px, &px])
                    .arg(&src)
                    .arg("--out")
                    .arg(&dst)
                    .output()
                    .context("running sips")?;
                if !o.status.success() {
                    bail!("sips failed: {}", String::from_utf8_lossy(&o.stderr).trim());
                }
            }
        }
        let o = Command::new("iconutil")
            .args(["-c", "icns", "-o"])
            .arg(out)
            .arg(&set)
            .output()
            .context("running iconutil")?;
        if !o.status.success() {
            bail!("iconutil failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        Ok(())
    })();
    fs::remove_dir_all(&tmp).ok();
    result
}

pub fn install(exe: &Path) -> Result<String> {
    let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let s = exe.to_string_lossy();
    let warn = if s.contains("/target/debug/") || s.contains("/target/release/") {
        format!("\nwarning: points at a build-dir binary ({s}); install tandm first and re-run from there")
    } else {
        String::new()
    };
    match std::env::consts::OS {
        "macos" => {
            let app = macos_bundle();
            if app.exists() && !app.join("Contents/MacOS/tandm").is_symlink() {
                bail!("{} exists and wasn't made by tandm; move it away first", app.display());
            }
            let contents = app.join("Contents");
            let macos = contents.join("MacOS");
            let res = contents.join("Resources");
            fs::create_dir_all(&macos)?;
            fs::create_dir_all(&res)?;
            util::write_atomic(
                &contents.join("Info.plist"),
                info_plist(env!("CARGO_PKG_VERSION")).as_bytes(),
            )?;
            let link = macos.join("tandm");
            fs::remove_file(&link).ok();
            std::os::unix::fs::symlink(&exe, &link)
                .with_context(|| format!("linking {}", link.display()))?;
            let icon_note = match make_icns(&res.join("tandm.icns")) {
                Ok(()) => String::new(),
                Err(e) => format!(" (no icon: {e:#})"),
            };
            // Make Finder/Dock/Spotlight pick up the (new) bundle and icon now.
            Command::new("touch").arg(&app).status().ok();
            Command::new(
                "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
            )
            .args(["-f"])
            .arg(&app)
            .output()
            .ok();
            Ok(format!(
                "installed {} -> {}{icon_note}{warn}",
                app.display(),
                exe.display()
            ))
        }
        "linux" => {
            let icon = linux_icon();
            fs::create_dir_all(icon.parent().unwrap())?;
            util::write_atomic(&icon, ICON_SVG)?;
            let d = desktop_file();
            fs::create_dir_all(d.parent().unwrap())?;
            util::write_atomic(&d, desktop_entry(&exe).as_bytes())?;
            Command::new("update-desktop-database")
                .arg(d.parent().unwrap())
                .output()
                .ok();
            Ok(format!("installed {} -> {}{warn}", d.display(), exe.display()))
        }
        os => bail!("`tandm app` isn't implemented on {os} yet"),
    }
}

pub fn uninstall() -> Result<String> {
    match std::env::consts::OS {
        "macos" => {
            let app = macos_bundle();
            if !app.exists() {
                return Ok(format!("{} not installed", app.display()));
            }
            if !app.join("Contents/MacOS/tandm").is_symlink() {
                bail!("{} wasn't made by tandm; leaving it", app.display());
            }
            fs::remove_dir_all(&app).with_context(|| format!("removing {}", app.display()))?;
            Ok(format!("removed {}", app.display()))
        }
        "linux" => {
            fs::remove_file(desktop_file()).ok();
            fs::remove_file(linux_icon()).ok();
            Ok(format!("removed {}", desktop_file().display()))
        }
        os => bail!("`tandm app` isn't implemented on {os} yet"),
    }
}
