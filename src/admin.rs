//! A tiny HTTP server for the configuration page: `http://127.0.0.1:<port>/`.
//!
//! It is deliberately plain — one `TcpListener`, one thread per request, no
//! external web dependency — because it only ever serves one local page.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::service;
use crate::settings::Settings;
use crate::{app, diag};

/// Bumped into the page so the browser toolbar shows something recognisable.
const PAGE: &str = include_str!("../assets/admin.html");

/// Guards against a random web page poking this local server: browsers cannot
/// add a custom header to a cross-origin request without a preflight, and we
/// never answer `OPTIONS` with CORS permissions.
const GUARD_HEADER: &str = "x-dsh-launcher";

static LAST: Mutex<Option<(bool, String)>> = Mutex::new(None);

/// True while an accept loop owns the port.
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn start(preferred: u16) -> Result<u16, String> {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return Ok(app::admin_port());
    }
    let listener = match bind(preferred) {
        Ok(listener) => listener,
        Err(error) => {
            ACTIVE.store(false, Ordering::SeqCst);
            return Err(error);
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        ACTIVE.store(false, Ordering::SeqCst);
        return Err(format!("配置页端口设置失败：{error}"));
    }
    let port = listener
        .local_addr()
        .map_err(|error| format!("读取配置页端口失败：{error}"))?
        .port();
    app::set_admin_port(port);
    app::set_admin_on(true);
    diag::write(&format!("config page listening on http://127.0.0.1:{port}/"));

    std::thread::spawn(move || {
        // A non-blocking accept keeps the loop responsive, so `stop` can drop
        // the listener (and free the port) within a fraction of a second.
        while ACTIVE.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    std::thread::spawn(move || handle(stream));
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(80));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(120)),
            }
        }
        app::set_admin_on(false);
        diag::write("config page stopped, the port is released");
    });
    Ok(port)
}

/// Stops the accept loop and waits until the listening socket is really gone.
pub fn stop() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while app::admin_on() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Prefers the configured port, then walks upward, then asks the OS.
fn bind(preferred: u16) -> Result<TcpListener, String> {
    let mut ports: Vec<u16> = Vec::new();
    ports.push(preferred);
    for offset in 1..=20u32 {
        if let Some(port) = (preferred as u32).checked_add(offset) {
            if port <= u16::MAX as u32 {
                ports.push(port as u16);
            }
        }
    }
    ports.push(0);
    let mut last_error = String::from("没有可用的端口");
    for port in ports {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                if port != preferred {
                    diag::write(&format!(
                        "port {preferred} was busy, the config page uses {port} instead"
                    ));
                }
                return Ok(listener);
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(format!("配置页端口绑定失败：{last_error}"))
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn handle(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let request = match read_request(&mut stream) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(_) => {
            let _ = respond(&mut stream, 400, "text/plain; charset=utf-8", b"bad request");
            return;
        }
    };
    if !app::admin_enabled() {
        let _ = respond(
            &mut stream,
            503,
            "application/json; charset=utf-8",
            br#"{"error":"configuration page is switched off"}"#,
        );
        return;
    }
    route(&mut stream, request);
}

fn read_request(stream: &mut TcpStream) -> Result<Option<Request>, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = find_headers_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            let body = &buffer[end + 4..];
            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap_or_default();
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let target = parts.next().unwrap_or_default().to_string();
            if method.is_empty() || target.is_empty() {
                return Err("bad request line".to_string());
            }
            let (path, query) = split_target(&target);
            let mut headers = HashMap::new();
            for line in lines {
                if let Some((name, value)) = line.split_once(':') {
                    headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                }
            }
            let length: usize = headers
                .get("content-length")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            if length > 256 * 1024 {
                return Err("body too large".to_string());
            }
            let mut body = body.to_vec();
            while body.len() < length {
                let read = stream.read(&mut chunk).map_err(|e| e.to_string())?;
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read]);
            }
            return Ok(Some(Request {
                method,
                path,
                query,
                headers,
                body,
            }));
        }
        let read = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 256 * 1024 {
            return Err("request too large".to_string());
        }
    }
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let mut query = HashMap::new();
    let (path, raw) = match target.split_once('?') {
        Some((path, raw)) => (path, raw),
        None => (target, ""),
    };
    for pair in raw.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            query.insert(key.to_string(), percent_decode(value));
        }
    }
    (path.to_string(), query)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let text = &value[index + 1..index + 3];
                match u8::from_str_radix(text, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn host_is_local(request: &Request) -> bool {
    match request.headers.get("host") {
        Some(host) => {
            let name = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
            name.eq_ignore_ascii_case("127.0.0.1") || name.eq_ignore_ascii_case("localhost")
        }
        None => true,
    }
}

fn guarded(request: &Request) -> bool {
    request.headers.contains_key(GUARD_HEADER)
}

fn route(stream: &mut TcpStream, request: Request) {
    if !host_is_local(&request) {
        let _ = respond(stream, 403, "text/plain; charset=utf-8", b"forbidden host");
        return;
    }
    let method = request.method.as_str();
    let path = request.path.as_str();

    match (method, path) {
        ("GET", "/") => {
            let _ = respond(stream, 200, "text/html; charset=utf-8", PAGE.as_bytes());
        }
        ("GET", "/api/state") => respond_json(stream, 200, state()),
        ("GET", "/api/settings") => respond_json(stream, 200, json!(app::settings())),
        ("GET", "/api/log") => {
            let lines = request
                .query
                .get("lines")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(200);
            let name = request.query.get("name").map(String::as_str).unwrap_or("tray");
            let text = match name {
                "service" => tail_text(service_log_path(), lines),
                _ => tail_text(diag_path(), lines),
            };
            let _ = respond(stream, 200, "text/plain; charset=utf-8", text.as_bytes());
        }
        ("POST", "/api/settings") => {
            if !guarded(&request) {
                respond_json(stream, 403, json!({ "error": "缺少安全头" }));
                return;
            }
            let mut value: Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(error) => {
                    respond_json(stream, 400, json!({ "error": format!("配置格式不对：{error}") }));
                    return;
                }
            };
            if let Some(object) = value.as_object_mut() {
                if !object.contains_key("adminEnabled") {
                    object.insert(
                        "adminEnabled".to_string(),
                        json!(app::settings().admin_enabled),
                    );
                }
            }
            let settings: Settings = match serde_json::from_value(value) {
                Ok(settings) => settings,
                Err(error) => {
                    respond_json(stream, 400, json!({ "error": format!("配置格式不对：{error}") }));
                    return;
                }
            };
            let previous = app::settings();
            match app::set_settings(settings.clone()) {
                Ok(()) => {
                    diag::write(&format!(
                        "settings saved: port={} adminPort={} autostart={}",
                        settings.port, settings.admin_port, settings.autostart
                    ));
                    let restart_required =
                        settings.admin_port != previous.admin_port
                            || settings.admin_enabled != previous.admin_enabled;
                    let mut body = state();
                    body["restartRequired"] = json!(restart_required);
                    respond_json(stream, 200, body);
                }
                Err(error) => {
                    respond_json(stream, 400, json!({ "error": error }));
                }
            }
        }
        ("POST", "/api/action") => {
            if !guarded(&request) {
                respond_json(stream, 403, json!({ "error": "缺少安全头" }));
                return;
            }
            let parsed: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let action = parsed
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !matches!(action.as_str(), "start" | "restart" | "stop") {
                respond_json(stream, 400, json!({ "error": "未知操作" }));
                return;
            }
            if !app::try_begin() {
                respond_json(stream, 409, json!({ "error": "已经有一个操作在进行，请稍等" }));
                return;
            }
            diag::write(&format!("config page asked for {action}"));
            std::thread::spawn(move || run_action(&action));
            respond_json(stream, 202, json!({ "accepted": true }));
        }
        ("POST", "/api/open") => {
            if !guarded(&request) {
                respond_json(stream, 403, json!({ "error": "缺少安全头" }));
                return;
            }
            let url = resolve_url();
            let opened = service::open_url(&url);
            respond_json(stream, 200, json!({ "url": url, "opened": opened }));
        }
        ("POST", "/api/quit") => {
            if !guarded(&request) {
                respond_json(stream, 403, json!({ "error": "缺少安全头" }));
                return;
            }
            let parsed: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let stop_service = parsed
                .get("stopService")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            diag::write(if stop_service {
                "config page asked to stop the service and quit"
            } else {
                "config page asked to quit"
            });
            respond_json(stream, 200, json!({ "accepted": true }));
            std::thread::spawn(move || {
                if stop_service {
                    let _ = service::stop(&app::config());
                }
                std::thread::sleep(Duration::from_millis(150));
                crate::tray::request_quit();
                std::thread::sleep(Duration::from_millis(1200));
                std::process::exit(0);
            });
        }
        _ => respond_json(stream, 404, json!({ "error": "not found" })),
    }
}

fn run_action(action: &str) {
    let config = app::config();
    let outcome: Result<String, String> = match action {
        "start" => service::start(&config).map(|report| {
            format!(
                "服务已启动（{:.1}s，pid {}）",
                report.waited.as_secs_f32(),
                report.pid
            )
        }),
        "restart" => service::restart(&config).map(|report| {
            format!(
                "服务已重启（{:.1}s，pid {}）",
                report.waited.as_secs_f32(),
                report.pid
            )
        }),
        _ => service::stop(&config).map(|pids| {
            if pids.is_empty() {
                "服务本来就没在运行".to_string()
            } else {
                format!("已停止（结束 {} 个进程）", pids.len())
            }
        }),
    };

    let message = match outcome {
        Ok(message) => {
            if action != "stop" {
                let url = service::authenticated_url(&config);
                if !url.ends_with('/') {
                    app::set_url(Some(url));
                }
            } else {
                app::set_url(None);
            }
            diag::write(&format!("config page action finished: {message}"));
            (true, message)
        }
        Err(error) => {
            diag::write(&format!("config page action failed: {error}"));
            (false, error)
        }
    };
    if let Ok(mut slot) = LAST.lock() {
        *slot = Some(message);
    }
    app::finish();
}

/// Records the outcome of an action triggered from the tray menu, so the page
/// shows the same message the balloon did.
pub fn record(ok: bool, message: &str) {
    if let Ok(mut slot) = LAST.lock() {
        *slot = Some((ok, message.to_string()));
    }
}

fn resolve_url() -> String {
    if let Some(url) = app::url() {
        return url;
    }
    let config = app::config();
    let url = service::authenticated_url(&config);
    if !url.ends_with('/') {
        app::set_url(Some(url.clone()));
    }
    url
}

fn state() -> Value {
    let settings = app::settings();
    let config = app::config();
    let running = service::is_running(config.port);
    let pids: Vec<u32> = if running {
        service::listener_pids(config.port)
    } else {
        Vec::new()
    };
    let last = LAST
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .map(|(ok, message)| json!({ "ok": ok, "message": message }));
    json!({
        "running": running,
        "pids": pids,
        "busy": app::busy(),
        "url": app::url(),
        "adminPort": app::admin_port(),
        "adminUrl": app::admin_url(),
        "logDir": settings.log_dir().to_string_lossy(),
        "settingsFile": Settings::file().to_string_lossy(),
        "command": service::preview(&config),
        "settings": settings,
        "last": last,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn respond_json(stream: &mut TcpStream, status: u16, value: Value) {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    let _ = respond(stream, status, "application/json; charset=utf-8", &body);
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn diag_path() -> Option<PathBuf> {
    diag::path()
}

fn service_log_path() -> Option<PathBuf> {
    let dir = app::settings().log_dir();
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if !name.starts_with("web-") || !name.ends_with(".out.log") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if newest.as_ref().map(|(time, _)| modified > *time).unwrap_or(true) {
                    newest = Some((modified, path));
                }
            }
        }
    }
    newest.map(|(_, path)| path)
}

fn tail_text(path: Option<PathBuf>, lines: usize) -> String {
    let Some(path) = path else {
        return "（还没有日志）".to_string();
    };
    let text = read_shared(&path).unwrap_or_default();
    let lines = lines.clamp(1, 5000);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// The service keeps its log open, so a plain read can fail with a sharing
/// violation; retry with share-read/write access.
fn read_shared(path: &Path) -> Option<String> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x7)
        .open(path)
        .ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}
