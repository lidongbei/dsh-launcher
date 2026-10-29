//! Talking to `dsh web`: is it up, how do we start it, how do we stop it, and
//! which address did it print at startup.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
use windows::core::PCWSTR;

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How the launcher is told which `dsh web` to run.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub node: Option<PathBuf>,
    pub entry: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    pub ready_timeout: Duration,
    pub stop_timeout: Duration,
    pub profile: String,
    pub patches: Vec<String>,
    pub public_url: String,
    pub trusted_hosts: Vec<String>,
    pub tls_cert: String,
    pub tls_key: String,
    pub extra_args: Vec<String>,
    /// `--max-old-space-size=<heap_mb>`, 0 leaves Node's default heap.
    pub heap_mb: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 3080,
            node: None,
            entry: None,
            log_dir: None,
            ready_timeout: Duration::from_secs(120),
            stop_timeout: Duration::from_secs(30),
            profile: "web".to_string(),
            patches: Vec::new(),
            public_url: String::new(),
            trusted_hosts: Vec::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            extra_args: Vec::new(),
            heap_mb: 0,
        }
    }
}

impl Config {
    /// The profile name handed to `dsh`, never empty.
    pub fn profile_name(&self) -> &str {
        let trimmed = self.profile.trim();
        if trimmed.is_empty() { "web" } else { trimmed }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}/", self.host, self.port)
    }

    pub fn log_dir(&self) -> PathBuf {
        if let Some(dir) = &self.log_dir {
            return dir.clone();
        }
        home_dir().join(".dsh").join("logs")
    }
}

/// The exact argument list `start` uses, kept in one place so the preview the
/// configuration page shows cannot drift from reality.
fn append_arguments(command: &mut Command, config: &Config, entry: &Path) {
    if config.heap_mb > 0 {
        command.arg(format!("--max-old-space-size={}", config.heap_mb));
    }
    command.arg(entry).arg(config.profile_name());
    for patch in &config.patches {
        command.arg("--patch").arg(patch);
    }
    command
        .arg("--host")
        .arg(&config.host)
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--no-open");
    if !config.public_url.is_empty() {
        command.arg("--public-url").arg(&config.public_url);
    }
    for authority in &config.trusted_hosts {
        command.arg("--trusted-host").arg(authority);
    }
    if !config.tls_cert.is_empty() {
        command.arg("--tls-cert").arg(&config.tls_cert);
        command.arg("--tls-key").arg(&config.tls_key);
    }
    for extra in &config.extra_args {
        command.arg(extra);
    }
}

/// Mirrors `append_arguments` as a single readable line.
pub fn preview(config: &Config) -> String {
    let entry = match &config.entry {
        Some(path) => path.to_string_lossy().to_string(),
        None => "bin.js".to_string(),
    };
    let mut parts = vec!["node".to_string()];
    if config.heap_mb > 0 {
        parts.push(format!("--max-old-space-size={}", config.heap_mb));
    }
    parts.push(entry);
    parts.push(config.profile_name().to_string());
    for patch in &config.patches {
        parts.push("--patch".to_string());
        parts.push(patch.clone());
    }
    parts.push("--host".to_string());
    parts.push(config.host.clone());
    parts.push("--port".to_string());
    parts.push(config.port.to_string());
    parts.push("--no-open".to_string());
    if !config.public_url.is_empty() {
        parts.push("--public-url".to_string());
        parts.push(config.public_url.clone());
    }
    for authority in &config.trusted_hosts {
        parts.push("--trusted-host".to_string());
        parts.push(authority.clone());
    }
    if !config.tls_cert.is_empty() {
        parts.push("--tls-cert".to_string());
        parts.push(config.tls_cert.clone());
        parts.push("--tls-key".to_string());
        parts.push(config.tls_key.clone());
    }
    for extra in &config.extra_args {
        parts.push(extra.clone());
    }
    parts.join(" ")
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn now_stamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    millis.to_string()
}

// ---------------------------------------------------------------- status

/// True when something accepts a TCP connection on the loopback port.
pub fn is_running(port: u16) -> bool {
    let address = format!("127.0.0.1:{port}");
    let Ok(mut addrs) = address.to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// PIDs that own a listening socket on `port` (IPv4 and IPv6).
pub fn listener_pids(port: u16) -> Vec<u32> {
    let mut pids = Vec::new();
    pids.extend(ipv4_listeners(port));
    pids.extend(ipv6_listeners(port));
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn read_table(family: u32) -> Option<Vec<u8>> {
    let mut size: u32 = 0;
    loop {
        let code = unsafe {
            GetExtendedTcpTable(
                None,
                &mut size,
                false,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if code == 0 {
            return None;
        }
        if code != ERROR_INSUFFICIENT_BUFFER || size == 0 {
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        let code = unsafe {
            GetExtendedTcpTable(
                Some(buffer.as_mut_ptr() as *mut _),
                &mut size,
                false,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if code == 0 {
            buffer.truncate(size as usize);
            return Some(buffer);
        }
        if code != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
    }
}

fn u32_at(buffer: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&buffer[offset..offset + 4]);
    u32::from_ne_bytes(bytes)
}

fn port_of(raw: u32) -> u16 {
    let value = raw & 0xFFFF;
    (((value & 0xFF) << 8) | ((value >> 8) & 0xFF)) as u16
}

fn ipv4_listeners(port: u16) -> Vec<u32> {
    let Some(buffer) = read_table(AF_INET) else {
        return Vec::new();
    };
    if buffer.len() < 4 {
        return Vec::new();
    }
    let count = u32_at(&buffer, 0) as usize;
    let row_size = std::mem::size_of::<MIB_TCPTABLE_OWNER_PID>() - std::mem::size_of::<u32>();
    let _ = row_size;
    // MIB_TCPROW_OWNER_PID is 24 bytes: state, local addr, local port, remote addr,
    // remote port, owning pid.
    let mut pids = Vec::new();
    for index in 0..count {
        let base = 4 + index * 24;
        if base + 24 > buffer.len() {
            break;
        }
        let local_port = port_of(u32_at(&buffer, base + 8));
        let owner = u32_at(&buffer, base + 20);
        if local_port == port {
            pids.push(owner);
        }
    }
    pids
}

fn ipv6_listeners(port: u16) -> Vec<u32> {
    let Some(buffer) = read_table(AF_INET6) else {
        return Vec::new();
    };
    if buffer.len() < 4 {
        return Vec::new();
    }
    let count = u32_at(&buffer, 0) as usize;
    // MIB_TCP6ROW_OWNER_PID is 56 bytes; local port sits at offset 20, owner at 52.
    let mut pids = Vec::new();
    for index in 0..count {
        let base = 4 + index * 56;
        if base + 56 > buffer.len() {
            break;
        }
        let local_port = port_of(u32_at(&buffer, base + 20));
        let owner = u32_at(&buffer, base + 52);
        if local_port == port {
            pids.push(owner);
        }
    }
    pids
}

// ---------------------------------------------------------------- processes

fn snapshot_processes() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return map;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    map
}

/// Every descendant of `root` (children first), so a parent cannot respawn them.
fn descendants(root: u32) -> Vec<u32> {
    let parents = snapshot_processes();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, parent) in parents {
        children.entry(parent).or_default().push(pid);
    }
    let mut ordered = Vec::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if let Some(kids) = children.get(&pid) {
            for kid in kids {
                stack.push(*kid);
                ordered.push(*kid);
            }
        }
    }
    ordered.reverse();
    ordered.push(root);
    ordered
}

fn terminate(pid: u32) -> bool {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) else {
            return false;
        };
        let ok = TerminateProcess(handle, 1).is_ok();
        let _ = CloseHandle(handle);
        ok
    }
}

/// Stops whatever listens on the configured port, children included.
pub fn stop(config: &Config) -> Result<Vec<u32>, String> {
    let pids = listener_pids(config.port);
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let mut stopped = Vec::new();
    for pid in &pids {
        for target in descendants(*pid) {
            if terminate(target) {
                stopped.push(target);
            }
        }
    }
    let deadline = Instant::now() + config.stop_timeout;
    while Instant::now() < deadline {
        if !is_running(config.port) {
            return Ok(stopped);
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    Err(format!(
        "port {} is still busy after {:?}",
        config.port, config.stop_timeout
    ))
}

// ---------------------------------------------------------------- install

/// Where `node.exe` and the dsh entry script live.
#[derive(Debug, Clone)]
pub struct Installation {
    pub node: PathBuf,
    pub entry: PathBuf,
}

pub fn find_installation(config: &Config) -> Result<Installation, String> {
    if let (Some(node), Some(entry)) = (&config.node, &config.entry) {
        return Ok(Installation {
            node: node.clone(),
            entry: entry.clone(),
        });
    }
    if let (Some(node), Some(entry)) = (env_path("DSH_NODE"), env_path("DSH_ENTRY")) {
        return Ok(Installation { node, entry });
    }

    let mut candidates: Vec<Installation> = Vec::new();

    // 1. the node builds the SDK keeps under %USERPROFILE%\.sdk\cache\node
    let cache = home_dir().join(".sdk").join("cache").join("node");
    if let Ok(families) = fs::read_dir(&cache) {
        let mut found: Vec<(String, Installation)> = Vec::new();
        for family in families.flatten() {
            let Ok(versions) = fs::read_dir(family.path()) else {
                continue;
            };
            for version in versions.flatten() {
                let root = version.path();
                let node = root.join("node.exe");
                let entry = root
                    .join("node_modules")
                    .join("@deepseek-ai")
                    .join("dsh")
                    .join("lib")
                    .join("bin.js");
                if node.is_file() && entry.is_file() {
                    let key = version.file_name().to_string_lossy().to_string();
                    found.push((key, Installation { node, entry }));
                }
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        candidates.extend(found.into_iter().map(|(_, install)| install).rev());
    }

    // 2. whatever `node` is on PATH, plus the usual global install roots
    if let Some(node) = which("node.exe") {
        for entry in global_entry_candidates() {
            if entry.is_file() {
                candidates.push(Installation {
                    node: node.clone(),
                    entry,
                });
            }
        }
    }

    candidates
        .into_iter()
        .find(|install| install.node.is_file() && install.entry.is_file())
        .ok_or_else(|| {
            "could not find node.exe plus @deepseek-ai/dsh/lib/bin.js; pass --node and --entry"
                .to_string()
        })
}

fn global_entry_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(pf).join("nodejs"));
    }
    roots
        .into_iter()
        .map(|root| {
            root.join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js")
        })
        .collect()
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

fn which(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ---------------------------------------------------------------- start

pub struct StartReport {
    pub pid: u32,
    pub waited: Duration,
}

pub fn start(config: &Config) -> Result<StartReport, String> {
    if is_running(config.port) {
        return Err(format!("already running on port {}", config.port));
    }
    let install = find_installation(config)?;
    let log_dir = config.log_dir();
    fs::create_dir_all(&log_dir)
        .map_err(|e| format!("cannot create {}: {e}", log_dir.display()))?;

    let stamp = now_stamp();
    let out_log = log_dir.join(format!("web-{stamp}.out.log"));
    let err_log = log_dir.join(format!("web-{stamp}.err.log"));
    let out_file =
        File::create(&out_log).map_err(|e| format!("cannot open {}: {e}", out_log.display()))?;
    let err_file =
        File::create(&err_log).map_err(|e| format!("cannot open {}: {e}", err_log.display()))?;

    let mut command = Command::new(&install.node);
    append_arguments(&mut command, config, &install.entry);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", install.node.display()))?;
    let pid = child.id();

    let deadline = Instant::now() + config.ready_timeout;
    while Instant::now() < deadline {
        if is_running(config.port) {
            return Ok(StartReport {
                pid,
                waited: started.elapsed(),
            });
        }
        if let Ok(Some(status)) = child.try_wait() {
            let tail = tail_of(&err_log, 20);
            let reason = tail
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("Error:"))
                .or_else(|| tail.lines().map(str::trim).find(|line| !line.is_empty()))
                .unwrap_or("");
            return Err(format!(
                "dsh web exited early with {status}: {reason}\n{}",
                tail.trim_end()
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    Err(format!(
        "dsh web did not open port {} within {:?}",
        config.port, config.ready_timeout
    ))
}

pub fn restart(config: &Config) -> Result<StartReport, String> {
    stop(config)?;
    start(config)
}

fn tail_of(path: &Path, lines: usize) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let collected: Vec<&str> = text.lines().rev().take(lines).collect();
    collected.into_iter().rev().collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------- token url

/// The `?token=` addresses dsh printed into its newest startup logs.
pub fn printed_urls(config: &Config) -> Vec<String> {
    let dir = config.log_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut logs: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("web-") || !name.ends_with(".out.log") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        logs.push((modified, entry.path()));
    }
    logs.sort_by(|a, b| b.0.cmp(&a.0));

    let needle_port = format!(":{}", config.port);
    let mut urls: Vec<String> = Vec::new();
    for (_, path) in logs.iter().take(40) {
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            continue;
        }
        for piece in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
            if !piece.starts_with("http") || !piece.contains("token=") {
                continue;
            }
            let url = piece.trim_end_matches([',', ')', ']', '>']).to_string();
            // The printed address may be a public URL or an https one, but the
            // token itself also works on the loopback address, so try both.
            if let Some(token) = url.split("token=").nth(1) {
                let token = token.split(['&', '#']).next().unwrap_or(token);
                let loopback = format!("{}?token={}", config.base_url(), token);
                if !urls.contains(&loopback) {
                    urls.push(loopback);
                }
            }
            if url.contains(&needle_port) && !urls.contains(&url) {
                urls.push(url);
            }
            if urls.len() >= 5 {
                return urls;
            }
        }
    }
    urls
}

/// A printed address is good when the server answers anything but 401.
pub fn url_accepted(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let host = authority.split(':').next().unwrap_or(authority);
    let port = authority
        .split(':')
        .nth(1)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(80);
    let address = format!("{host}:{port}");
    let Ok(mut addrs) = address.to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(800)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(2500)));
    let request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    let mut buffer = [0u8; 512];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                response.push_str(&String::from_utf8_lossy(&buffer[..count]));
                if response.contains("\r\n") || response.len() > 256 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok());
    matches!(status, Some(code) if code != 401)
}

/// The address worth opening: a printed token url the server still accepts,
/// otherwise the plain base address.
pub fn authenticated_url(config: &Config) -> String {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if is_running(config.port) {
            for url in printed_urls(config) {
                if url_accepted(&url) {
                    return url;
                }
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(600));
    }
    config.base_url()
}

pub fn open_url(url: &str) -> bool {
    let operation = wide("open");
    let target = wide(url);
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            PCWSTR::from_raw(operation.as_ptr()),
            PCWSTR::from_raw(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    result.0 as isize > 32
}

pub fn open_folder(path: &Path) -> bool {
    let operation = wide("open");
    let target = wide(&path.to_string_lossy());
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            PCWSTR::from_raw(operation.as_ptr()),
            PCWSTR::from_raw(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    result.0 as isize > 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_mirrors_the_arguments() {
        let config = Config {
            port: 3096,
            public_url: "https://app.example/ui/".to_string(),
            trusted_hosts: vec!["app.example".to_string()],
            patches: vec!["overlays/extra.yml".to_string()],
            extra_args: vec!["--log-level".to_string(), "debug".to_string()],
            ..Config::default()
        };
        let preview = preview(&config);
        assert!(
            preview.contains(" web --patch overlays/extra.yml"),
            "{preview}"
        );
        assert!(preview.contains("--port 3096"), "{preview}");
        assert!(
            preview.contains("--public-url https://app.example/ui/"),
            "{preview}"
        );
        assert!(preview.contains("--trusted-host app.example"), "{preview}");
        assert!(preview.ends_with("--log-level debug"), "{preview}");
    }

    #[test]
    fn an_empty_profile_falls_back_to_web() {
        let config = Config {
            profile: "   ".to_string(),
            ..Config::default()
        };
        assert_eq!(config.profile_name(), "web");
        assert!(preview(&config).contains(" bin.js web "));
    }
}
