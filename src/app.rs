//! Process-wide state shared by the tray icon, the command line and the
//! configuration page: the current settings, the cached tokenised address, and
//! the "an action is running right now" flag.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::service::Config;
use crate::settings::Settings;

struct App {
    settings: Mutex<Settings>,
    url: Mutex<Option<String>>,
    busy: AtomicBool,
    admin_port: AtomicU16,
    admin_on: AtomicBool,
    /// PID of the `dsh web` process the tray is waiting on, 0 when none.
    service_pid: AtomicU32,
    service_started: AtomicU64,
}

static APP: OnceLock<App> = OnceLock::new();

fn app() -> &'static App {
    APP.get_or_init(|| {
        let settings = Settings::load();
        App {
            admin_port: AtomicU16::new(settings.admin_port),
            admin_on: AtomicBool::new(false),
            settings: Mutex::new(settings),
            url: Mutex::new(None),
            busy: AtomicBool::new(false),
            service_pid: AtomicU32::new(0),
            service_started: AtomicU64::new(0),
        }
    })
}

pub fn settings() -> Settings {
    app().settings.lock().unwrap().clone()
}

/// Stores settings for this run only (command line overrides).
pub fn publish(settings: Settings) {
    let admin_port = settings.admin_port;
    *app().settings.lock().unwrap() = settings;
    app().admin_port.store(admin_port, Ordering::SeqCst);
}

/// Validates, persists and publishes new settings.
pub fn set_settings(next: Settings) -> Result<(), String> {
    next.validate()?;
    next.save()?;
    *app().settings.lock().unwrap() = next.clone();
    set_url(None);
    Ok(())
}

pub fn config() -> Config {
    settings().service_config()
}

pub fn url() -> Option<String> {
    app().url.lock().unwrap().clone()
}

pub fn set_url(url: Option<String>) {
    *app().url.lock().unwrap() = url;
}

pub fn busy() -> bool {
    app().busy.load(Ordering::SeqCst)
}

/// Claims the single action slot; returns false when one is already running.
pub fn try_begin() -> bool {
    app()
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

pub fn finish() {
    app().busy.store(false, Ordering::SeqCst);
}

/// PID of the service process the tray is currently watching (0 = none).
pub fn service_pid() -> u32 {
    app().service_pid.load(Ordering::SeqCst)
}

/// Forgets the watched process, including its generation.
pub fn set_service_pid(pid: u32) {
    app().service_pid.store(pid, Ordering::SeqCst);
    if pid == 0 {
        app().service_started.store(0, Ordering::SeqCst);
    }
}

pub fn set_service_started(ticks: u64) {
    app().service_started.store(ticks, Ordering::SeqCst);
}

/// Whether the configuration page server is running in this process.
pub fn admin_on() -> bool {
    app().admin_on.load(Ordering::SeqCst)
}

/// Whether the user wants the configuration page (the tray menu checkbox).
pub fn admin_enabled() -> bool {
    app().settings.lock().unwrap().admin_enabled
}

/// The port the page is actually served on (may differ from the setting when
/// the configured port was taken).
pub fn set_admin_on(on: bool) {
    app().admin_on.store(on, Ordering::SeqCst);
}

pub fn admin_port() -> u16 {
    app().admin_port.load(Ordering::SeqCst)
}

pub fn set_admin_port(port: u16) {
    app().admin_port.store(port, Ordering::SeqCst);
}

pub fn admin_url() -> String {
    format!("http://127.0.0.1:{}/", admin_port())
}
