//! Embeds the launcher icon into the executable. The Windows SDK's `rc.exe` is
//! usually not on PATH, so its directory is prepended before compiling.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/launcher.rc");
    println!("cargo:rerun-if-changed=assets/launcher.ico");

    if let Some(dir) = find_rc_dir() {
        if let Some(existing) = std::env::var_os("PATH") {
            let mut paths = vec![dir];
            paths.extend(std::env::split_paths(&existing));
            if let Ok(joined) = std::env::join_paths(paths) {
                // SAFETY: build scripts are single threaded here, and PATH is
                // only read afterwards by the resource compiler.
                unsafe { std::env::set_var("PATH", joined) };
            }
        }
    }

    let _ = embed_resource::compile("assets/launcher.rc", embed_resource::NONE);
}

fn find_rc_dir() -> Option<PathBuf> {
    let root = Path::new("C:/Program Files (x86)/Windows Kits/10/bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    versions.sort();
    for version in versions.iter().rev() {
        for arch in ["x64", "x86"] {
            let candidate = version.join(arch).join("rc.exe");
            if candidate.is_file() {
                return candidate.parent().map(Path::to_path_buf);
            }
        }
    }
    None
}
