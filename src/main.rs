//! A tray launcher for `dsh web`: one executable, no dependencies at runtime.
//!
//! * double click or `dsh-launcher.exe` -> tray icon only, no console window
//! * `dsh-launcher.exe -Action Status` -> one line on the console, then exit
//! * the tray also serves a configuration page on http://127.0.0.1:3089/

#![cfg_attr(windows, windows_subsystem = "windows")]

mod admin;
mod app;
mod diag;
mod icon;
mod service;
mod settings;
mod tray;

use std::path::PathBuf;

use settings::Settings;

const HELP: &str = r#"DSH Launcher -- dsh web 的托盘启动器

用法:
  dsh-launcher.exe                         显示托盘图标（默认启动服务）
  dsh-launcher.exe -Action Status          打印运行状态（运行中 exit 0）
  dsh-launcher.exe -Action Start           启动服务后退出
  dsh-launcher.exe -Action Restart         重启服务后退出
  dsh-launcher.exe -Action Stop            停止服务后退出
  dsh-launcher.exe -Action Url             打印带 token 的访问地址
  dsh-launcher.exe -Action Admin           打开配置页
  dsh-launcher.exe -Action Icon -Out <目录> 导出四种状态的图标预览

可选项（仅对本次运行生效，配置页里保存的才是持久的）:
  -Port <端口>            默认 3080
  -Host <地址>            默认 127.0.0.1
  -HeapMb <MB>            传给 node 的 --max-old-space-size，默认 0（不设置）
  -NodeExe <路径>         指定 node.exe（默认自动查找）
  -EntryScript <路径>     指定 dsh 的 bin.js（默认自动查找）
  -LogDirectory <目录>    服务日志目录，默认 %USERPROFILE%\.dsh\logs
  -ReadyTimeout <秒>      等待服务就绪的超时，默认 120
  -StopTimeout <秒>       等待服务退出的超时，默认 30
  -AdminPort <端口>       配置页端口，默认 3089
  -NoAdmin                不启动配置页
  -Out <目录>             -Action Icon 的导出目录
  -NoAutoStart            打开托盘时不自动启动服务
  -NoDetach               兼容旧脚本的空开关
  -Help                   显示这段帮助

配置文件: %LOCALAPPDATA%\DshLauncher\settings.json
配置页:   http://127.0.0.1:<AdminPort>/
"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Start,
    Restart,
    Stop,
    Status,
    Url,
    Icon,
    Admin,
}

struct Options {
    settings: Settings,
    action: Option<Action>,
    out: Option<PathBuf>,
    admin: bool,
    help: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let settings = Settings::load();
        let mut options = Options {
            settings: settings.clone(),
            action: None,
            out: None,
            admin: settings.admin_enabled,
            help: false,
        };

        let mut args = args.peekable();
        while let Some(raw) = args.next() {
            let key = raw.trim_start_matches(['-', '/']).to_ascii_lowercase();
            let mut take_value = |label: &str| -> Result<String, String> {
                args.next().ok_or_else(|| format!("参数 {label} 缺少取值"))
            };
            match key.as_str() {
                "action" | "a" => {
                    let text = take_value(&raw)?;
                    options.action = Some(match text.to_ascii_lowercase().as_str() {
                        "start" => Action::Start,
                        "restart" => Action::Restart,
                        "stop" => Action::Stop,
                        "status" => Action::Status,
                        "url" => Action::Url,
                        "icon" => Action::Icon,
                        "admin" | "config" => Action::Admin,
                        other => return Err(format!("未知的 -Action：{other}")),
                    });
                }
                "port" | "p" => {
                    let text = take_value(&raw)?;
                    options.settings.port =
                        text.parse().map_err(|_| format!("端口不是数字：{text}"))?;
                }
                "host" => options.settings.host = take_value(&raw)?,
                "heapmb" => {
                    let text = take_value(&raw)?;
                    options.settings.heap_mb = text
                        .parse()
                        .map_err(|_| format!("堆内存上限不是数字：{text}"))?;
                }
                "nodeexe" => options.settings.node_exe = take_value(&raw)?,
                "entryscript" => options.settings.entry_script = take_value(&raw)?,
                "logdirectory" => options.settings.log_directory = take_value(&raw)?,
                "readytimeout" => {
                    let text = take_value(&raw)?;
                    options.settings.ready_timeout =
                        text.parse().map_err(|_| format!("超时不是整数：{text}"))?;
                }
                "stoptimeout" => {
                    let text = take_value(&raw)?;
                    options.settings.stop_timeout =
                        text.parse().map_err(|_| format!("超时不是整数：{text}"))?;
                }
                "adminport" => {
                    let text = take_value(&raw)?;
                    options.settings.admin_port = text
                        .parse()
                        .map_err(|_| format!("配置页端口不是数字：{text}"))?;
                }
                "out" | "o" => options.out = Some(PathBuf::from(take_value(&raw)?)),
                "noautostart" => options.settings.autostart = false,
                "noadmin" => {
                    options.admin = false;
                    options.settings.admin_enabled = false;
                }
                // The Rust build never detaches, so this only keeps shortcuts
                // made for the PowerShell version working.
                "nodetach" | "detached" => {}
                "environmentfile" => {
                    let _ = take_value(&raw)?;
                }
                "help" | "h" | "?" => options.help = true,
                other => return Err(format!("未知参数：-{other}")),
            }
        }
        Ok(options)
    }
}

/// Prints one line and tolerates a closed pipe (`| head`, or a captured stream
/// that went away) — `println!` panics in that case.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = writeln!(out, $($arg)*);
    }};
}

macro_rules! complain {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let mut out = std::io::stderr();
        let _ = writeln!(out, $($arg)*);
    }};
}

fn main() {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            attach_console();
            complain!("{error}");
            complain!("用 -Help 查看用法");
            std::process::exit(2);
        }
    };

    if options.help {
        attach_console();
        print!("{HELP}");
        return;
    }

    let _ = diag::init();
    app::publish(options.settings.clone());

    if let Some(action) = options.action {
        attach_console();
        let code = run_action(action, options.out.as_deref());
        std::process::exit(code);
    }

    if !acquire_single_instance(options.settings.port) {
        // A tray is already up; a second icon would only confuse the user.
        return;
    }

    if options.admin
        && options.settings.admin_enabled
        && let Err(error) = admin::start(options.settings.admin_port)
    {
        diag::write(&format!("config page failed to start: {error}"));
    }

    let result = tray::run(tray::TrayOptions {
        autostart: options.settings.autostart,
    });
    if let Err(error) = result {
        show_error(&error);
        std::process::exit(1);
    }
}

fn run_action(action: Action, out: Option<&std::path::Path>) -> i32 {
    let config = app::config();
    match action {
        Action::Status => {
            if service::is_running(config.port) {
                let pids = service::listener_pids(config.port);
                if pids.is_empty() {
                    say!("running  {}", config.base_url());
                } else {
                    let list: Vec<String> = pids.iter().map(u32::to_string).collect();
                    say!("running  {}  (pid {})", config.base_url(), list.join(", "));
                }
                0
            } else {
                say!("stopped  (port {})", config.port);
                1
            }
        }
        Action::Url => {
            let url = service::authenticated_url(&config);
            if url.ends_with('/') {
                say!("no token yet: {url}");
                1
            } else {
                say!("{url}");
                0
            }
        }
        Action::Admin => {
            let url = app::admin_url();
            let opened = service::open_url(&url);
            say!("{url}");
            if opened {
                0
            } else {
                complain!("浏览器没有打开配置页");
                1
            }
        }
        Action::Icon => {
            let dir = out
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("icons"));
            match icon::dump_states(&dir, &[16, 32, 48]) {
                Ok(paths) => {
                    for path in paths {
                        say!("{}", path.display());
                    }
                    0
                }
                Err(error) => {
                    complain!("icon dump failed: {error}");
                    1
                }
            }
        }
        Action::Start => match service::start(&config) {
            Ok(report) => {
                say!(
                    "started  {}  ({:.1}s, pid {})",
                    config.base_url(),
                    report.waited.as_secs_f32(),
                    report.pid
                );
                0
            }
            Err(error) => {
                complain!("start failed: {error}");
                1
            }
        },
        Action::Restart => match service::restart(&config) {
            Ok(report) => {
                say!(
                    "restarted  {}  ({:.1}s, pid {})",
                    config.base_url(),
                    report.waited.as_secs_f32(),
                    report.pid
                );
                0
            }
            Err(error) => {
                complain!("restart failed: {error}");
                1
            }
        },
        Action::Stop => match service::stop(&config) {
            Ok(pids) => {
                if pids.is_empty() {
                    say!("stopped  (nothing was listening on {})", config.port);
                } else {
                    say!("stopped  ({} process(es) ended)", pids.len());
                }
                0
            }
            Err(error) => {
                complain!("stop failed: {error}");
                1
            }
        },
    }
}

fn acquire_single_instance(port: u16) -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    let name: Vec<u16> = format!("Local\\DshLauncher_{port}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr())) };
    match handle {
        Ok(_) => (unsafe { GetLastError() }).0 != ERROR_ALREADY_EXISTS.0,
        Err(_) => true,
    }
}

fn show_error(message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;

    let text = HSTRING::from(message);
    unsafe {
        MessageBoxW(
            None,
            &text,
            &HSTRING::from("DSH Launcher"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn attach_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
