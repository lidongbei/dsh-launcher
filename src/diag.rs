//! A very small append-only log, so a tray that misbehaves can be explained
//! later. The file lives under `%LOCALAPPDATA%\DshLauncher\tray.log`.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Points the log at `%LOCALAPPDATA%\DshLauncher\tray.log` and returns its path.
pub fn init() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("DshLauncher");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("tray.log");
    if let Ok(mut slot) = LOG_PATH.lock() {
        *slot = Some(path.clone());
    }
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > 512 * 1024 {
            let _ = fs::rename(&path, dir.join("tray.log.old"));
        }
    }
    path
}

pub fn path() -> Option<PathBuf> {
    LOG_PATH.lock().ok().and_then(|slot| slot.clone())
}

pub fn write(message: &str) {
    let path = match LOG_PATH.lock() {
        Ok(slot) => slot.clone(),
        Err(_) => None,
    };
    let Some(path) = path else {
        return;
    };
    let stamp = timestamp();
    let line = format!("{stamp} {message}\n");
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write as _;
        let _ = file.write_all(line.as_bytes());
    }
}

fn timestamp() -> String {
    let time = unsafe { GetLocalTime() };
    let mut text = String::new();
    let _ = write!(
        text,
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    );
    text
}
