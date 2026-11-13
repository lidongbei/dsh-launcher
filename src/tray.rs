//! The notification-area shell: one hidden window, one icon, a popup menu, and
//! worker threads so a slow start never freezes the menu.

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::admin;
use crate::app;
use crate::diag;
use crate::icon::{self, State};
use crate::service;

const WM_TRAY: u32 = WM_APP + 1;
const WM_DONE: u32 = WM_APP + 2;
/// A watched `dsh web` process ended; `wparam` carries its PID.
const WM_SERVICE_EXIT: u32 = WM_APP + 3;
/// The watcher's settle window ran out: the service really is gone.
const WM_SETTLE: u32 = WM_APP + 4;
/// The stopped-state re-check timer beat.
const WM_WATCHDOG: u32 = WM_APP + 5;

const TRAY_ID: u32 = 1;

const ID_STATUS: usize = 1000;
const ID_OPEN: usize = 1001;
const ID_OPEN_ADMIN: usize = 1008;
const ID_ADMIN: usize = 1009;
const ID_START: usize = 1002;
const ID_RESTART: usize = 1003;
const ID_STOP: usize = 1004;
const ID_LOGS: usize = 1005;
const ID_EXIT: usize = 1006;
const ID_EXIT_STOP: usize = 1007;

/// Workers only need the window handle, so it is kept as a plain integer.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// Finished work lands here; the window proc drains it.
static RESULTS: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());
/// Bumped on every `dsh web` exit we learn about, so the settle poll started
/// for the previous process gives up instead of reporting the new one.
static EXITS: AtomicUsize = AtomicUsize::new(0);
/// A settle thread is running; a second one would only race it.
static SETTLING: AtomicBool = AtomicBool::new(false);
/// True once this process owns the tray. A second tray must not watch the same
/// service: it would announce the same exit and interfere with the first.
static PRIMARY: AtomicBool = AtomicBool::new(false);

/// A worker finished: 0 = success, 1 = failure, 2 = a resolved address.
const RESULT_OK: i32 = 0;
const RESULT_ERROR: i32 = 1;
const RESULT_URL: i32 = 2;

/// How long the watcher keeps looking for a replacement process once the one
/// it waited on is gone (`dsh` may restart itself by re-executing), and how
/// often it looks while it does.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(8);
const SETTLE_STEP: std::time::Duration = std::time::Duration::from_millis(120);
/// A probe connects in microseconds while something listens; a closed port on
/// this machine swallows the SYN, so keep the budget far below the 300 ms one.
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(120);

struct Tray {
    menu: HMENU,
    icons: Vec<(State, HICON)>,
    state: State,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct TrayOptions {
    pub autostart: bool,
}

pub fn run(options: TrayOptions) -> Result<(), String> {
    let _ = diag::init();
    let instance: HINSTANCE = unsafe {
        GetModuleHandleW(None)
            .map_err(|e| format!("GetModuleHandleW failed: {e}"))?
            .into()
    };

    let class_name = wide("DshLauncherTrayWindow");
    let window_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        return Err(format!(
            "RegisterClassW failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let title = wide("DSH Launcher");
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .map_err(|e| format!("CreateWindowExW failed: {e}"))?
    };
    WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);

    let first_tray = only_instance();
    PRIMARY.store(first_tray, Ordering::SeqCst);
    if first_tray {
        start_watchdog();
    } else {
        // A second tray (Start with Windows firing again) exists: it keeps its
        // icon, but leaves the watching to the one that owns the tray.
        diag::write("another tray already owns the port; this one does not watch");
    }

    let size = icon::icon_size();
    let icons = icon::build_set(size)?;
    let menu = build_menu()?;

    let config = app::config();
    let state = if service::is_running_with(config.port, PROBE_BUDGET) {
        State::Running
    } else {
        State::Stopped
    };
    if state == State::Running {
        // adopt an instance someone else started (a terminal, `npx`, ...)
        watch_current();
    }

    TRAY.with(|slot| {
        *slot.borrow_mut() = Some(Tray { menu, icons, state });
    });

    diag::write(&format!(
        "tray started: state={} port={}",
        state.slug(),
        config.port
    ));
    add_icon(hwnd);
    refresh_icon(hwnd);

    if options.autostart && state == State::Stopped {
        begin(Action::Start);
    }

    // dsh prints its tokenised address a few seconds after the port opens, so
    // resolve it in the background instead of blocking the first menu click
    {
        let config = app::config();
        let hwnd_raw = hwnd.0 as isize;
        std::thread::spawn(move || {
            let url = service::authenticated_url(&config);
            RESULTS.lock().unwrap().push((RESULT_URL, url));
            let hwnd = HWND(hwnd_raw as *mut _);
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_DONE, WPARAM(0), LPARAM(0));
            }
        });
    }

    message_loop();
    Ok(())
}
fn message_loop() {
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn build_menu() -> Result<HMENU, String> {
    unsafe {
        let menu = CreatePopupMenu().map_err(|e| format!("CreatePopupMenu failed: {e}"))?;
        let labels: Vec<Vec<u16>> = [
            "状态：未知",
            "打开 Web UI",
            "打开配置页",
            "启用配置页",
            "启动服务",
            "重启服务",
            "停止服务",
            "打开日志目录",
            "退出托盘（保留服务）",
            "停止服务并退出",
        ]
        .iter()
        .map(|text| wide(text))
        .collect();
        let item = |flags: MENU_ITEM_FLAGS, id: usize, index: usize| {
            let _ = AppendMenuW(menu, flags, id, PCWSTR::from_raw(labels[index].as_ptr()));
        };
        item(MF_STRING | MF_GRAYED, ID_STATUS, 0);
        item(MF_SEPARATOR, 0, 0);
        item(MF_STRING, ID_OPEN, 1);
        item(MF_STRING, ID_OPEN_ADMIN, 2);
        item(MF_STRING, ID_ADMIN, 3);
        item(MF_SEPARATOR, 0, 0);
        item(MF_STRING, ID_START, 4);
        item(MF_STRING, ID_RESTART, 5);
        item(MF_STRING, ID_STOP, 6);
        item(MF_SEPARATOR, 0, 0);
        item(MF_STRING, ID_LOGS, 7);
        item(MF_SEPARATOR, 0, 0);
        item(MF_STRING, ID_EXIT, 8);
        item(MF_STRING, ID_EXIT_STOP, 9);
        Ok(menu)
    }
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ID;
    data
}

fn with_tray<T>(f: impl FnOnce(&Tray) -> T, fallback: T) -> T {
    TRAY.with(|slot| match slot.borrow().as_ref() {
        Some(tray) => f(tray),
        None => fallback,
    })
}

fn with_tray_mut(f: impl FnOnce(&mut Tray)) {
    TRAY.with(|slot| {
        if let Some(tray) = slot.borrow_mut().as_mut() {
            f(tray);
        }
    });
}

fn tooltip(state: State) -> String {
    let config = app::config();
    format!(
        "DSH Web · {}:{} · {}",
        config.host,
        config.port,
        state.label()
    )
}

impl Tray {
    fn handles(&self) -> Vec<HICON> {
        self.icons.iter().map(|(_, handle)| *handle).collect()
    }
}

fn icon_of(state: State) -> HICON {
    with_tray(
        |tray| {
            tray.icons
                .iter()
                .find(|(candidate, _)| *candidate == state)
                .map(|(_, handle)| *handle)
                .unwrap_or_default()
        },
        HICON::default(),
    )
}

fn add_icon(hwnd: HWND) {
    let state = with_tray(|tray| tray.state, State::Unknown);
    let mut data = notify_data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = icon_of(state);
    data.szTip = tip_text(&tooltip(state));
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
    }
}

fn remove_icon(hwnd: HWND) {
    let data = notify_data(hwnd);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

fn tip_text(text: &str) -> [u16; 128] {
    let mut target = [0u16; 128];
    let encoded: Vec<u16> = text.encode_utf16().take(126).collect();
    target[..encoded.len()].copy_from_slice(&encoded);
    target
}

fn refresh_icon(hwnd: HWND) {
    let state = with_tray(|tray| tray.state, State::Unknown);
    let mut data = notify_data(hwnd);
    data.uFlags = NIF_ICON | NIF_TIP;
    data.hIcon = icon_of(state);
    data.szTip = tip_text(&tooltip(state));
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
    update_menu_state();
}

fn update_menu_state() {
    let (menu, state, busy) = with_tray(
        |tray| (tray.menu, tray.state, app::busy()),
        (HMENU::default(), State::Unknown, true),
    );
    if menu.is_invalid() {
        return;
    }
    let label = wide(&format!("状态：{}", state.label()));
    let running = state == State::Running;
    unsafe {
        let _ = ModifyMenuW(
            menu,
            ID_STATUS as u32,
            MF_BYCOMMAND | MF_STRING | MF_GRAYED,
            ID_STATUS,
            PCWSTR::from_raw(label.as_ptr()),
        );
        enable(menu, ID_START, !running && !busy);
        enable(menu, ID_RESTART, running && !busy);
        enable(menu, ID_STOP, running && !busy);
        enable(menu, ID_OPEN, true);
        enable(menu, ID_OPEN_ADMIN, app::admin_on());
        let ticked = if app::admin_enabled() {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        let _ = CheckMenuItem(menu, ID_ADMIN as u32, (MF_BYCOMMAND | ticked).0);
        enable(menu, ID_LOGS, true);
    }
}

unsafe fn enable(menu: HMENU, id: usize, enabled: bool) {
    unsafe {
        let flags = if enabled {
            MF_BYCOMMAND | MF_ENABLED
        } else {
            MF_BYCOMMAND | MF_GRAYED
        };
        let _ = EnableMenuItem(menu, id as u32, flags);
    }
}

fn balloon(hwnd: HWND, title: &str, text: &str, warning: bool) {
    let mut data = notify_data(hwnd);
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
    let title_wide: Vec<u16> = title.encode_utf16().take(60).collect();
    let text_wide: Vec<u16> = text.encode_utf16().take(240).collect();
    data.szInfoTitle[..title_wide.len()].copy_from_slice(&title_wide);
    data.szInfo[..text_wide.len()].copy_from_slice(&text_wide);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

#[derive(Clone, Copy)]
enum Action {
    Start,
    Restart,
    Stop,
}

/// Waits on the process that owns the port and posts `WM_SERVICE_EXIT` when it
/// dies, so the icon turns red the moment the service goes away instead of on
/// the next menu click.
///
/// The owner, not our direct child: with `npx` the listener is a grandchild.
fn watch_service(pid: u32) {
    if pid == 0 || pid == app::service_pid() || !PRIMARY.load(Ordering::SeqCst) {
        return;
    }
    let hwnd_raw = WINDOW.load(Ordering::SeqCst);
    if hwnd_raw == 0 {
        return;
    }
    let key = service::process_key(pid);
    app::set_service_pid(pid);
    app::set_service_started(key.map(|key| key.started).unwrap_or(0));
    diag::write(&format!("watching service process {pid}"));
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut _);
        let exited = match key {
            Some(key) => service::wait_for_key(key),
            None => service::wait_for_exit(pid),
        };
        if exited {
            report_exit(hwnd, pid);
        }
    });
}

fn report_exit(hwnd: HWND, pid: u32) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_SERVICE_EXIT, WPARAM(pid as usize), LPARAM(0));
    }
}

/// The single tray: a second launch (Start with Windows firing twice, a user
/// double-click) keeps its icon but must not watch the same service.
const INSTANCE: &str = "Local\\DshLauncherTray";

fn only_instance() -> bool {
    let name = wide(INSTANCE);
    let created = unsafe { CreateMutexW(None, false, PCWSTR::from_raw(name.as_ptr())) };
    match created {
        Ok(_) => unsafe { GetLastError() != ERROR_ALREADY_EXISTS },
        Err(_) => true,
    }
}

/// Polls while the port is still held after the watched process is gone: `dsh`
/// may restart itself behind the same port, or a new instance may take over.
/// Returns true when the port came back, false when nothing answers until the
/// settle window runs out (or an action takes over).
fn settle_and_announce(port: u16, pid: u32, generation: usize) -> bool {
    let deadline = std::time::Instant::now() + SETTLE;
    loop {
        if EXITS.load(Ordering::SeqCst) != generation || app::busy() {
            return false;
        }
        if let Some(owner) = service::owner_pid(port).filter(|owner| *owner != pid) {
            // someone listens again: follow the new process instead of
            // blinking the icon through a false "stopped"
            watch_service(owner);
            diag::write(&format!("service is back on port {port} as pid {owner}"));
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(SETTLE_STEP);
    }
}

/// Starts the settle poll for a process the tray was watching. Runs off the
/// window thread; the window thread does the announcing.
fn begin_settle(hwnd: HWND, pid: u32) {
    if SETTLING.swap(true, Ordering::SeqCst) {
        return;
    }
    let generation = EXITS.fetch_add(1, Ordering::SeqCst) + 1;
    let port = app::config().port;
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let came_back = settle_and_announce(port, pid, generation);
        if !came_back {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd_raw as *mut _)),
                    WM_SETTLE,
                    WPARAM(pid as usize),
                    LPARAM(0),
                );
            }
        }
        SETTLING.store(false, Ordering::SeqCst);
    });
}

/// Watches whatever currently listens on the port, if anything.
fn watch_current() {
    let config = app::config();
    if let Some(pid) = service::owner_pid(config.port) {
        watch_service(pid);
    }
}

fn begin(action: Action) {
    let config = app::config();
    if !app::try_begin() {
        return;
    }
    with_tray_mut(|tray| tray.state = State::Working);
    let hwnd_raw = WINDOW.load(Ordering::SeqCst);
    let hwnd = HWND(hwnd_raw as *mut _);
    refresh_icon(hwnd);
    diag::write(&format!(
        "action {} started on port {}",
        match action {
            Action::Start => "start",
            Action::Restart => "restart",
            Action::Stop => "stop",
        },
        config.port
    ));

    std::thread::spawn(move || {
        let outcome = match action {
            Action::Start => service::start(&config).map(|report| {
                format!(
                    "已启动 {}（{:.1}s，pid {}）",
                    config.base_url(),
                    report.waited.as_secs_f32(),
                    report.pid
                )
            }),
            Action::Restart => service::restart(&config).map(|report| {
                format!(
                    "已重启 {}（{:.1}s，pid {}）",
                    config.base_url(),
                    report.waited.as_secs_f32(),
                    report.pid
                )
            }),
            Action::Stop => service::stop(&config).map(|pids| {
                if pids.is_empty() {
                    "服务本来就没在运行".to_string()
                } else {
                    format!("已停止（结束 {} 个进程）", pids.len())
                }
            }),
        };
        let entry = match outcome {
            Ok(text) => {
                diag::write(&format!("action finished: {text}"));
                (RESULT_OK, text)
            }
            Err(error) => {
                diag::write(&format!("action failed: {error}"));
                (RESULT_ERROR, error)
            }
        };
        RESULTS.lock().unwrap().push(entry);
        let hwnd = HWND(hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_DONE, WPARAM(0), LPARAM(0));
        }
    });
}

fn drain_results(hwnd: HWND) {
    let finished: Vec<(i32, String)> = {
        let mut queue = RESULTS.lock().unwrap();
        std::mem::take(&mut *queue)
    };
    for (code, text) in finished {
        match code {
            RESULT_URL => {
                app::set_url(Some(text));
            }
            RESULT_OK | RESULT_ERROR => {
                app::finish();
                admin::record(code == RESULT_OK, &text);
                if code == RESULT_OK {
                    // a restarted service prints a fresh token, so drop the cache
                    app::set_url(None);
                    let config = app::config();
                    let hwnd_raw = WINDOW.load(Ordering::SeqCst);
                    std::thread::spawn(move || {
                        let url = service::authenticated_url(&config);
                        RESULTS.lock().unwrap().push((RESULT_URL, url));
                        let hwnd = HWND(hwnd_raw as *mut _);
                        unsafe {
                            let _ = PostMessageW(Some(hwnd), WM_DONE, WPARAM(0), LPARAM(0));
                        }
                    });
                }
                let running = app::config();
                let state = if service::is_running_with(running.port, PROBE_BUDGET) {
                    State::Running
                } else {
                    State::Stopped
                };
                with_tray_mut(|tray| tray.state = state);
                if state == State::Running {
                    watch_current();
                } else {
                    // the process is gone: forget the watcher and the token URL
                    app::set_service_pid(0);
                    app::set_url(None);
                }
                refresh_icon(hwnd);
                if code == RESULT_OK {
                    balloon(hwnd, "DSH Web", &text, false);
                } else {
                    balloon(hwnd, "操作失败", &text, true);
                }
            }
            _ => {}
        }
    }
}

/// A tray that believes the service is stopped re-checks the port every few
/// seconds. Nothing ticks while the icon is green (the wait handle covers
/// that), and a stopped tray costs one connect with a 120 ms cap per beat:
/// measured at ~0.1 % of one core, because a live port answers at once.
const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(3);

fn start_watchdog() {
    let hwnd_raw = WINDOW.load(Ordering::SeqCst);
    if hwnd_raw == 0 {
        return;
    }
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(WATCHDOG);
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd_raw as *mut _)),
                    WM_WATCHDOG,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
}

fn watchdog_beat(hwnd: HWND) {
    if app::busy() || SETTLING.load(Ordering::SeqCst) {
        return;
    }
    if app::service_pid() != 0 || with_tray(|tray| tray.state, State::Unknown) == State::Running {
        // the wait handle reports this death; no need to probe
        return;
    }
    adopt_if_back(hwnd);
}

/// Spots a service that came back without the tray being told: restarted from
/// another terminal, started again by us while we were told to stop waiting,
/// or one the settle window missed. Only runs on the window thread and only
/// while nothing else is going on, so it never fights an action.
fn adopt_if_back(hwnd: HWND) {
    if app::busy() || app::service_pid() != 0 {
        return;
    }
    let state = with_tray(|tray| tray.state, State::Unknown);
    if state == State::Running {
        return;
    }
    let config = app::config();
    if !service::is_running_with(config.port, PROBE_BUDGET) {
        return;
    }
    diag::write(&format!("service is back on port {}", config.port));
    with_tray_mut(|tray| tray.state = State::Running);
    refresh_icon(hwnd);
    watch_current();
}

/// Marks the service as stopped once the watcher is sure nothing answers any
/// more: repaint, drop the tokenised address, and say it out loud unless the
/// tray already showed a stop.
fn announce_stopped(hwnd: HWND, gone: Option<u32>) {
    if service::is_running_with(app::config().port, PROBE_BUDGET) {
        // something listens again (possibly already watched): follow it
        watch_current();
        return;
    }
    let previous = with_tray(|tray| tray.state, State::Unknown);
    with_tray_mut(|tray| tray.state = State::Stopped);
    refresh_icon(hwnd);
    app::set_url(None);
    // The balloon is what tells a user who is not watching the icon; a tray
    // that already showed red has said it once and stays quiet.
    if previous != State::Stopped && previous != State::Working {
        let mut data = notify_data(hwnd);
        data.uFlags = NIF_INFO | NIF_ICON | NIF_TIP;
        data.hIcon = icon_of(State::Stopped);
        data.szTip = tip_text(&tooltip(State::Stopped));
        data.dwInfoFlags = NIIF_WARNING;
        let title: Vec<u16> = "DSH Web".encode_utf16().collect();
        let text: Vec<u16> = match gone {
            Some(pid) => format!("服务已停止（进程 {pid} 退出后没有再起来）"),
            None => "服务已停止".to_string(),
        }
        .encode_utf16()
        .take(240)
        .collect();
        data.szInfoTitle[..title.len()].copy_from_slice(&title);
        data.szInfo[..text.len()].copy_from_slice(&text);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }
}

/// Re-reads the port and repaints the icon. Called when the menu opens and
/// after every action, so nothing ticks in the background.
fn refresh_state(hwnd: HWND) {
    if app::busy() {
        return;
    }
    let config = app::config();
    let previous = with_tray(|tray| tray.state, State::Unknown);
    let running = service::is_running_with(config.port, PROBE_BUDGET);
    if running {
        watch_current();
    } else {
        app::set_service_pid(0);
    }
    let state = if running {
        State::Running
    } else {
        State::Stopped
    };
    if state == previous {
        return;
    }
    with_tray_mut(|tray| tray.state = state);
    refresh_icon(hwnd);
    if state == State::Stopped && previous == State::Running {
        balloon(hwnd, "DSH Web", "服务已停止", true);
    }
}

/// Closes the tray from another thread (the configuration page uses it).
pub fn request_quit() {
    let raw = WINDOW.load(Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Flips the configuration page on or off and remembers the choice.
fn toggle_admin(hwnd: HWND) {
    let mut settings = app::settings();
    settings.admin_enabled = !settings.admin_enabled;
    let turning_on = settings.admin_enabled;
    if let Err(error) = app::set_settings(settings.clone()) {
        balloon(hwnd, "保存失败", &error, true);
        return;
    }
    diag::write(if turning_on {
        "config page switched on from the tray"
    } else {
        "config page switched off from the tray"
    });
    if turning_on && !app::admin_on() {
        match admin::start(settings.admin_port) {
            Ok(port) => balloon(
                hwnd,
                "配置页已开启",
                &format!("http://127.0.0.1:{port}/"),
                false,
            ),
            Err(error) => balloon(hwnd, "配置页启动失败", &error, true),
        }
    } else if turning_on {
        let url = app::admin_url();
        balloon(hwnd, "配置页已开启", &url, false);
    } else {
        admin::stop();
        let port = app::admin_port();
        balloon(
            hwnd,
            "配置页已关闭",
            &format!("端口 {port} 已释放，托盘菜单里可以再打开"),
            false,
        );
    }
    update_menu_state();
}

fn open_admin(hwnd: HWND) {
    if !app::admin_on() {
        balloon(
            hwnd,
            "配置页未启用",
            "托盘菜单里勾选「启用配置页」即可",
            true,
        );
        return;
    }
    let url = app::admin_url();
    diag::write(&format!("open config page requested: {url}"));
    if !service::open_url(&url) {
        balloon(hwnd, "打开失败", &url, true);
    }
}

fn open_ui(hwnd: HWND) {
    let (config, cached) = (app::config(), app::url());
    diag::write("open web ui requested");
    if let Some(url) = cached {
        if !service::open_url(&url) {
            balloon(hwnd, "打开失败", &url, true);
        }
        return;
    }
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let url = service::authenticated_url(&config);
        if url.ends_with('/') {
            // no tokenised address yet: tell the user instead of opening a 401
            let hwnd = HWND(hwnd_raw as *mut _);
            RESULTS.lock().unwrap().push((
                RESULT_ERROR,
                "服务刚启动时还没打印带 token 的地址，过几秒再点一次".to_string(),
            ));
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_DONE, WPARAM(0), LPARAM(0));
            }
            return;
        }
        let opened = service::open_url(&url);
        RESULTS.lock().unwrap().push((RESULT_URL, url.clone()));
        let hwnd = HWND(hwnd_raw as *mut _);
        if !opened {
            RESULTS
                .lock()
                .unwrap()
                .push((RESULT_ERROR, format!("浏览器没有打开 {url}")));
        }
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_DONE, WPARAM(0), LPARAM(0));
        }
    });
}

fn show_menu(hwnd: HWND) {
    adopt_if_back(hwnd);
    refresh_state(hwnd);
    let menu = with_tray(|tray| tray.menu, HMENU::default());
    if menu.is_invalid() {
        return;
    }
    update_menu_state();
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        if chosen.0 != 0 {
            handle_command(hwnd, chosen.0 as usize);
        }
    }
}

fn handle_command(hwnd: HWND, command: usize) {
    match command {
        ID_OPEN => open_ui(hwnd),
        ID_START => begin(Action::Start),
        ID_RESTART => begin(Action::Restart),
        ID_STOP => begin(Action::Stop),
        ID_OPEN_ADMIN => open_admin(hwnd),
        ID_ADMIN => toggle_admin(hwnd),
        ID_LOGS => {
            let dir = app::config().log_dir();
            let _ = std::fs::create_dir_all(&dir);
            if !service::open_folder(&dir) {
                balloon(hwnd, "打开失败", &dir.to_string_lossy(), true);
            }
        }
        ID_EXIT => {
            diag::write("tray exit requested, service keeps running");
            remove_icon(hwnd);
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        ID_EXIT_STOP => {
            diag::write("tray exit requested, stopping the service first");
            let config = app::config();
            let hwnd_raw = hwnd.0 as isize;
            std::thread::spawn(move || {
                let _ = service::stop(&config);
                let hwnd = HWND(hwnd_raw as *mut _);
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            });
        }
        _ => {}
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_TRAY => {
                let event = (lparam.0 as u32) & 0xFFFF;
                match event {
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    WM_LBUTTONDBLCLK => open_ui(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                handle_command(hwnd, wparam.0 & 0xFFFF);
                LRESULT(0)
            }
            WM_DONE => {
                drain_results(hwnd);
                LRESULT(0)
            }
            WM_SERVICE_EXIT => {
                let pid = wparam.0 as u32;
                if pid != 0 && pid == app::service_pid() {
                    app::set_service_pid(0);
                    diag::write(&format!("service process {pid} exited"));
                    // While an action runs, drain_results settles the state.
                    if !app::busy() {
                        begin_settle(hwnd, pid);
                    }
                }
                LRESULT(0)
            }
            WM_WATCHDOG => {
                watchdog_beat(hwnd);
                LRESULT(0)
            }
            WM_SETTLE => {
                // A watched process is gone and nothing has taken the port back
                // for a few seconds: say so instead of waiting for a click.
                if !app::busy() && app::service_pid() == 0 {
                    adopt_if_back(hwnd);
                    if app::service_pid() == 0 {
                        let gone = wparam.0 as u32;
                        diag::write(&format!("service process {gone} did not come back"));
                        announce_stopped(hwnd, Some(gone));
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                remove_icon(hwnd);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let (menu, icons) = with_tray(
                    |tray| (tray.menu, tray.handles()),
                    (HMENU::default(), Vec::new()),
                );
                if !menu.is_invalid() {
                    let _ = DestroyMenu(menu);
                }
                for handle in icons {
                    icon::destroy(handle);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}
