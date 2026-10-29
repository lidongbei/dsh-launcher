//! The notification-area shell: one hidden window, one icon, a popup menu, and
//! worker threads so a slow start never freezes the menu.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app;
use crate::diag;
use crate::admin;
use crate::icon::{self, State};
use crate::service;

const WM_TRAY: u32 = WM_APP + 1;
const WM_DONE: u32 = WM_APP + 2;
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

/// A worker finished: 0 = success, 1 = failure, 2 = a resolved address.
const RESULT_OK: i32 = 0;
const RESULT_ERROR: i32 = 1;
const RESULT_URL: i32 = 2;

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
        return Err("RegisterClassW failed".to_string());
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

    let size = icon::icon_size();
    let icons = icon::build_set(size)?;
    let menu = build_menu()?;

    let config = app::config();
    let state = if service::is_running(config.port) {
        State::Running
    } else {
        State::Stopped
    };

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
    format!("DSH Web · {}:{} · {}", config.host, config.port, state.label())
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
        let ticked = if app::admin_enabled() { MF_CHECKED } else { MF_UNCHECKED };
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
                let state = if service::is_running(running.port) {
                    State::Running
                } else {
                    State::Stopped
                };
                with_tray_mut(|tray| tray.state = state);
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

/// Re-reads the port and repaints the icon. Called when the menu opens and
/// after every action, so nothing ticks in the background.
fn refresh_state(hwnd: HWND) {
    if app::busy() {
        return;
    }
    let config = app::config();
    let previous = with_tray(|tray| tray.state, State::Unknown);
    let running = service::is_running(config.port);
    let state = if running { State::Running } else { State::Stopped };
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
            Ok(port) => balloon(hwnd, "配置页已开启", &format!("http://127.0.0.1:{port}/"), false),
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
        balloon(hwnd, "配置页未启用", "托盘菜单里勾选「启用配置页」即可", true);
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
            RESULTS
                .lock()
                .unwrap()
                .push((RESULT_ERROR, "服务刚启动时还没打印带 token 的地址，过几秒再点一次".to_string()));
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
        WM_CLOSE => {
            remove_icon(hwnd);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let (menu, icons) = with_tray(|tray| (tray.menu, tray.handles()), (HMENU::default(), Vec::new()));
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
