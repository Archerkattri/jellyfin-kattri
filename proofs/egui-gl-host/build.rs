//! Windows resource embedding for `jellyfin-kattri` (application icon and version info).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/windows/kattri.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let Ok(manifest_dir) = env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let Ok(out_dir) = env::var("OUT_DIR") else {
        return;
    };

    let icon_path = PathBuf::from(&manifest_dir)
        .join("..")
        .join("..")
        .join("packaging")
        .join("windows")
        .join("kattri.ico");
    let icon_path = fs::canonicalize(&icon_path).unwrap_or(icon_path);
    if !icon_path.exists() {
        println!(
            "cargo:warning=Windows icon not found at {}; skipping resource embedding",
            icon_path.display()
        );
        return;
    }

    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=rc.exe not found; skipping Windows resource embedding");
        return;
    };

    let rc_path = Path::new(&out_dir).join("kattri.rc");
    let res_path = Path::new(&out_dir).join("kattri.res");
    let rc_contents = build_rc_script(&icon_path);
    if let Err(error) = fs::write(&rc_path, rc_contents) {
        println!("cargo:warning=failed to write Windows .rc file: {error}");
        return;
    }

    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status();

    match status {
        Ok(exit) if exit.success() && res_path.exists() => {
            println!("cargo:rustc-link-arg-bins={}", res_path.display());
        }
        Ok(exit) => {
            println!(
                "cargo:warning=rc.exe ({}) exited with {exit}; skipping Windows resource embedding",
                rc_exe.display()
            );
        }
        Err(error) => {
            println!(
                "cargo:warning=failed to run rc.exe ({}): {error}; skipping Windows resource embedding",
                rc_exe.display()
            );
        }
    }
}

fn build_rc_script(icon_path: &Path) -> String {
    let raw = icon_path.to_string_lossy();
    let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let icon_escaped = trimmed.replace('\\', "/");
    let major = env::var("CARGO_PKG_VERSION_MAJOR").unwrap_or_else(|_| "0".to_owned());
    let minor = env::var("CARGO_PKG_VERSION_MINOR").unwrap_or_else(|_| "0".to_owned());
    let patch = env::var("CARGO_PKG_VERSION_PATCH").unwrap_or_else(|_| "0".to_owned());
    let version_str = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    let version_commas = format!("{major},{minor},{patch},0");

    format!(
        "IDI_ICON ICON \"{icon_escaped}\"\n\
         \n\
         1 VERSIONINFO\n\
         FILEVERSION {version_commas}\n\
         PRODUCTVERSION {version_commas}\n\
         FILEFLAGSMASK 0x3fL\n\
         FILEFLAGS 0x0L\n\
         FILEOS 0x40004L\n\
         FILETYPE 0x1L\n\
         FILESUBTYPE 0x0L\n\
         BEGIN\n\
             BLOCK \"StringFileInfo\"\n\
             BEGIN\n\
                 BLOCK \"040904B0\"\n\
                 BEGIN\n\
                     VALUE \"ProductName\", \"Jellyfin Kattri\"\n\
                     VALUE \"FileDescription\", \"Jellyfin Kattri Desktop Client\"\n\
                     VALUE \"FileVersion\", \"{version_str}\"\n\
                     VALUE \"ProductVersion\", \"{version_str}\"\n\
                     VALUE \"OriginalFilename\", \"jellyfin-kattri.exe\"\n\
                 END\n\
             END\n\
             BLOCK \"VarFileInfo\"\n\
             BEGIN\n\
                 VALUE \"Translation\", 0x409, 1200\n\
             END\n\
         END\n"
    )
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let entries = fs::read_dir(kits_bin).ok()?;
    let mut versions: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    versions.sort();
    versions.reverse();

    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => &["x86", "x64"][..],
        Ok("aarch64") => &["arm64", "x64"][..],
        _ => &["x64", "x86"][..],
    };

    for version_dir in versions {
        for subdir in arch {
            let candidate = version_dir.join(subdir).join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
