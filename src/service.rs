//! Talking to `dsh web`: is it up, how do we start it, how do we stop it, and
//! which address did it print at startup.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, FILETIME, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS,
    SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};
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
    /// Working directory of `dsh web`, which is the workspace its session
    /// commands run in. `None` falls back to the user's home directory.
    pub work_dir: Option<PathBuf>,
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
            work_dir: None,
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

    /// The directory `dsh web` starts in. dsh takes its own process cwd as the
    /// session workspace, so this is the directory the session's commands run
    /// against, not merely a place to keep files.
    pub fn session_dir(&self) -> PathBuf {
        match &self.work_dir {
            Some(dir) => dir.clone(),
            None => home_dir(),
        }
    }
}

/// The exact argument list `start` uses, kept in one place so the preview the
/// configuration page shows cannot drift from reality.
fn append_arguments(command: &mut Command, config: &Config, install: &Installation) {
    if config.heap_mb > 0 {
        if install.via_npx {
            // npx runs dsh in a child node process, so the limit has to travel
            // through the environment instead of node's own argument list.
            let inherited = std::env::var("NODE_OPTIONS").unwrap_or_default();
            let combined = format!("{inherited} --max-old-space-size={}", config.heap_mb);
            command.env("NODE_OPTIONS", combined.trim());
        } else {
            command.arg(format!("--max-old-space-size={}", config.heap_mb));
        }
    }
    command.arg(&install.script);
    for argument in &install.prefix {
        command.arg(argument);
    }
    command.arg(config.profile_name());
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
    // Discovery only decides the shape here: a direct install is shown as
    // `node bin.js`, the npx fallback as `node npx-cli.js -y <pkg>`.
    let via_npx = matches!(find_installation(config), Ok(install) if install.via_npx);
    let entry = match &config.entry {
        Some(path) => path.to_string_lossy().to_string(),
        None => "bin.js".to_string(),
    };
    let mut parts = vec!["node".to_string()];
    if config.heap_mb > 0 && !via_npx {
        parts.push(format!("--max-old-space-size={}", config.heap_mb));
    }
    if via_npx {
        parts.push("npx-cli.js".to_string());
        parts.push("-y".to_string());
        parts.push(NPX_PACKAGE.to_string());
    } else {
        parts.push(entry);
    }
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
    // The working directory is part of the command, because it decides where
    // dsh runs the session's commands.
    format!(
        "cd \"{}\" && {}",
        config.session_dir().display(),
        parts.join(" ")
    )
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
    is_running_with(port, Duration::from_millis(300))
}

/// Same probe with a caller-chosen budget. A tray sweep can afford to give up
/// sooner: a listening port answers in microseconds, and a dead one only costs
/// the budget because this machine silently drops the SYN.
pub fn is_running_with(port: u16, budget: Duration) -> bool {
    let address = format!("127.0.0.1:{port}");
    let Ok(mut addrs) = address.to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, budget).is_ok()
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

/// The process that owns the listening socket on `port`, if any.
pub fn owner_pid(port: u16) -> Option<u32> {
    listener_pids(port).first().copied()
}

/// Blocks in the kernel until `pid` exits, at zero CPU cost while waiting.
///
/// Returns false when the process cannot be opened (for example one running
/// elevated), so the caller can keep a probe as the fallback.
pub fn wait_for_exit(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return false;
        };
        let exited = WaitForSingleObject(handle, INFINITE) == WAIT_OBJECT_0;
        let _ = CloseHandle(handle);
        exited
    }
}

/// Like `wait_for_exit`, but also refuses to report a PID that now belongs to
/// a different process: it checks the start time even when the handle opens.
pub fn wait_for_key(key: ProcessKey) -> bool {
    if !wait_for_exit(key.pid) {
        return false;
    }
    // The handle was opened while the PID still carried this start time, and a
    // held handle keeps naming that process, so the exit it reports is ours.
    match process_key(key.pid) {
        Some(still) => still.started == key.started,
        None => true,
    }
}

/// What identifies one process instance: its PID and the moment it started.
///
/// Windows reuses PIDs, so a bare number can point at a different program
/// later. When the start time matches, the PID still names the same process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessKey {
    pub pid: u32,
    pub started: u64,
}

/// The signature of the live process `pid`, or None when it cannot be read.
pub fn process_key(pid: u32) -> Option<ProcessKey> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let read = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        if read.is_err() {
            return None;
        }
        let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        Some(ProcessKey {
            pid,
            started: ticks,
        })
    }
}

/// True while `pid` still carries that start time. Exited-but-unreaped
/// processes keep theirs, which is exactly what `wait_for_key` relies on.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_same_process(key: ProcessKey) -> bool {
    process_key(key.pid).is_some_and(|now| now.started == key.started)
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

/// How `dsh web` is reached: either a concrete `node.exe` + `bin.js` pair, or
/// npm's `npx` front end, which resolves (and on a cold cache downloads) the
/// package itself.
#[derive(Debug, Clone)]
pub struct Installation {
    pub node: PathBuf,
    /// The script handed to node: `bin.js`, or npm's `npx-cli.js`.
    pub script: PathBuf,
    /// Arguments between the script and the dsh arguments (`-y <pkg>` for npx).
    pub prefix: Vec<String>,
    /// True when npm resolves the package instead of running it in place.
    pub via_npx: bool,
}

const NPX_PACKAGE: &str = "@deepseek-ai/dsh";

impl Installation {
    fn direct(node: PathBuf, entry: PathBuf) -> Self {
        Self {
            node,
            script: entry,
            prefix: Vec::new(),
            via_npx: false,
        }
    }
}

pub fn find_installation(config: &Config) -> Result<Installation, String> {
    if let (Some(node), Some(entry)) = (&config.node, &config.entry) {
        return Ok(Installation::direct(node.clone(), entry.clone()));
    }
    if let (Some(node), Some(entry)) = (env_path("DSH_NODE"), env_path("DSH_ENTRY")) {
        return Ok(Installation::direct(node, entry));
    }

    // An explicit `-NodeExe` goes first, so it also drives the search below.
    let mut nodes = Vec::new();
    if let Some(node) = &config.node {
        nodes.push(node.clone());
    }
    for node in node_candidates() {
        if !nodes.contains(&node) {
            nodes.push(node);
        }
    }
    let mut candidates: Vec<Installation> = Vec::new();

    // 1. a node.exe sitting next to an unpacked @deepseek-ai/dsh: the SDK's
    //    cache layout, a portable install, or a node distribution that ships
    //    the package.
    for node in &nodes {
        let Some(directory) = node.parent() else {
            continue;
        };
        let entry = directory
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        if entry.is_file() {
            candidates.push(Installation::direct(node.clone(), entry));
        }
    }

    // 2. packages `npx @deepseek-ai/dsh` unpacked into npm's cache; the cache
    //    holds no node.exe, so pair them with the node found above.
    if let Some(node) = nodes.first() {
        for entry in npx_entry_candidates() {
            candidates.push(Installation::direct(node.clone(), entry));
        }
    }

    // 3. the usual global install roots next to the node on PATH
    if let Some(node) = nodes.first() {
        for entry in global_entry_candidates() {
            if entry.is_file() {
                candidates.push(Installation::direct(node.clone(), entry));
            }
        }
    }

    // An explicit `-EntryScript` pairs with the node found above.
    let explicit_entry = config.entry.as_ref().filter(|entry| entry.is_file());
    let explicit_node = nodes.first().filter(|node| node.is_file());
    if let (Some(entry), Some(node)) = (explicit_entry, explicit_node) {
        return Ok(Installation::direct(node.clone(), entry.clone()));
    }

    if let Some(found) = candidates
        .into_iter()
        .find(|install| install.node.is_file() && install.script.is_file())
    {
        return Ok(found);
    }

    // 4. nothing on disk: let npm fetch and run the package, exactly like a
    //    hand-typed `npx @deepseek-ai/dsh web`.
    if let Some(install) = npx_installation(nodes.first().cloned()) {
        return Ok(install);
    }

    Err(
        "could not find node.exe plus @deepseek-ai/dsh/lib/bin.js; install it with `npm i -g @deepseek-ai/dsh`, or pass --node and --entry"
            .to_string(),
    )
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

/// Every node.exe worth trying, most specific first: the SDK's cached builds
/// (newest first), then whatever PATH points at.
fn node_candidates() -> Vec<PathBuf> {
    let mut nodes = Vec::new();
    let cache = home_dir().join(".sdk").join("cache").join("node");
    if let Ok(families) = fs::read_dir(&cache) {
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        for family in families.flatten() {
            let Ok(versions) = fs::read_dir(family.path()) else {
                continue;
            };
            for version in versions.flatten() {
                let node = version.path().join("node.exe");
                if node.is_file() {
                    let key = version.file_name().to_string_lossy().to_string();
                    found.push((key, node));
                }
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        nodes.extend(found.into_iter().rev().map(|(_, node)| node));
    }
    if let Some(node) = which("node.exe").filter(|node| !nodes.contains(node)) {
        nodes.push(node);
    }
    nodes
}

/// `npx @deepseek-ai/dsh` unpacks the package under
/// `%LOCALAPPDATA%\npm-cache\_npx\<hash>` (npm 6 used `%APPDATA%\npm-cache`),
/// newest first.
fn npx_entry_candidates() -> Vec<PathBuf> {
    npx_entry_candidates_in(&npx_cache_roots())
}

fn npx_cache_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("npm-cache").join("_npx"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("npm-cache").join("_npx"));
    }
    roots
}

fn npx_entry_candidates_in(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut entries: Vec<(SystemTime, PathBuf)> = Vec::new();
    for root in roots {
        let Ok(directories) = fs::read_dir(root) else {
            continue;
        };
        for directory in directories.flatten() {
            let entry = directory
                .path()
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js");
            if !entry.is_file() {
                continue;
            }
            let stamp = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            entries.push((stamp, entry));
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries.into_iter().map(|(_, path)| path).collect()
}

/// Runs `node <npx-cli.js> -y @deepseek-ai/dsh …`: no .cmd shim, so no cmd.exe
/// layer and no quoting rules to get wrong.
fn npx_installation(node: Option<PathBuf>) -> Option<Installation> {
    let node = node?;
    let cli = npx_cli_candidates(&node)
        .into_iter()
        .find(|path| path.is_file())?;
    Some(Installation {
        node,
        script: cli,
        prefix: vec!["-y".to_string(), NPX_PACKAGE.to_string()],
        via_npx: true,
    })
}

fn npx_cli_candidates(node: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(directory) = node.parent() {
        roots.push(directory.to_path_buf());
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("nodejs"));
    }
    roots
        .into_iter()
        .map(|root| {
            root.join("node_modules")
                .join("npm")
                .join("bin")
                .join("npx-cli.js")
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

/// Spawns the service without handing it the launcher's own std handles.
///
/// Windows gives a child every inheritable handle we hold, even when its stdio
/// points at the log files. If the launcher was started inside a pipe (`… | cat`
/// or a script capturing output), the service would keep that pipe open for its
/// whole life and the caller would wait for an EOF that never arrives.
fn spawn_service(command: &mut Command) -> std::io::Result<Child> {
    let mut saved: Vec<(HANDLE, bool)> = Vec::new();
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let Ok(handle) = (unsafe { GetStdHandle(id) }) else {
            continue;
        };
        let mut flags = 0u32;
        if unsafe { GetHandleInformation(handle, &mut flags) }.is_err() {
            continue;
        }
        let inheritable = flags & HANDLE_FLAG_INHERIT.0 != 0;
        if inheritable {
            let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
        }
        saved.push((handle, inheritable));
    }
    let spawned = command.spawn();
    for (handle, inheritable) in saved {
        if inheritable {
            let _ =
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) };
        }
    }
    spawned
}

pub fn start(config: &Config) -> Result<StartReport, String> {
    if is_running(config.port) {
        return Err(format!("already running on port {}", config.port));
    }
    let install = find_installation(config)?;
    let session_dir = config.session_dir();
    if !session_dir.is_dir() {
        return Err(format!(
            "session directory {} does not exist",
            session_dir.display()
        ));
    }
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
    command.current_dir(&session_dir);
    append_arguments(&mut command, config, &install);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);

    let started = Instant::now();
    let mut child = spawn_service(&mut command)
        .map_err(|e| format!("cannot start {}: {e}", install.node.display()))?;
    let pid = child.id();

    let deadline = Instant::now() + config.ready_timeout;
    while Instant::now() < deadline {
        if is_running(config.port) {
            return Ok(StartReport {
                // The listener, not the launcher's direct child: with `npx` the
                // port is held by a grandchild, and this is the PID the tray
                // watches for the exit notification.
                pid: owner_pid(config.port).unwrap_or(pid),
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

    #[test]
    fn the_session_directory_falls_back_to_home() {
        assert_eq!(Config::default().session_dir(), home_dir());
        let config = Config {
            work_dir: Some(PathBuf::from("C:\\work\\project")),
            ..Config::default()
        };
        assert_eq!(config.session_dir(), PathBuf::from("C:\\work\\project"));
        assert!(
            preview(&config).starts_with("cd \"C:\\work\\project\" && node "),
            "{}",
            preview(&config)
        );
    }

    #[test]
    fn an_explicit_pair_is_used_as_is() {
        let config = Config {
            node: Some(PathBuf::from("C:\\tools\\node.exe")),
            entry: Some(PathBuf::from("C:\\tools\\bin.js")),
            ..Config::default()
        };
        let install = find_installation(&config).expect("explicit pair");
        assert!(!install.via_npx);
        assert_eq!(install.script, PathBuf::from("C:\\tools\\bin.js"));
        assert!(install.prefix.is_empty());
    }

    /// A bogus `-NodeExe` must not break discovery; whatever comes back has to
    /// exist on disk.
    #[test]
    fn a_missing_explicit_node_falls_back_to_discovery() {
        let config = Config {
            node: Some(PathBuf::from("C:\\nope\\node.exe")),
            ..Config::default()
        };
        if let Ok(install) = find_installation(&config) {
            assert!(install.node.is_file(), "{}", install.node.display());
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("dsh-launcher-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("scratch dir");
        directory
    }

    #[test]
    fn an_unpacked_npx_cache_entry_is_found() {
        let root = scratch_dir("npx-cache");
        let bin = root
            .join("0f9e2a")
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        fs::create_dir_all(bin.parent().expect("bin.js parent")).expect("cache layout");
        fs::write(&bin, "// stub").expect("bin.js");
        // something else in the cache must not be mistaken for dsh
        fs::create_dir_all(root.join("12ab34").join("node_modules").join("left-pad"))
            .expect("other package");

        assert_eq!(
            npx_entry_candidates_in(std::slice::from_ref(&root)),
            vec![bin]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_npx_fallback_points_at_npx_cli() {
        let directory = scratch_dir("npx-cli");
        let node = directory.join("node.exe");
        fs::write(&node, "").expect("node.exe stand-in");
        let cli = directory
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npx-cli.js");
        fs::create_dir_all(cli.parent().expect("npx-cli parent")).expect("npm layout");
        fs::write(&cli, "// stub").expect("npx-cli.js");

        let install = npx_installation(Some(node)).expect("npx fallback");
        assert!(install.via_npx);
        assert_eq!(install.script, cli);
        assert_eq!(install.prefix, ["-y".to_string(), NPX_PACKAGE.to_string()]);
        let _ = fs::remove_dir_all(&directory);
    }

    /// The npx shape: `node npx-cli.js -y @deepseek-ai/dsh web …`.
    #[test]
    fn an_npx_prefix_sits_before_the_dsh_arguments() {
        let install = Installation {
            node: PathBuf::from("node.exe"),
            script: PathBuf::from("npx-cli.js"),
            prefix: vec!["-y".to_string(), "@deepseek-ai/dsh".to_string()],
            via_npx: true,
        };
        let mut command = Command::new(&install.node);
        append_arguments(&mut command, &Config::default(), &install);
        let args: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..3], ["npx-cli.js", "-y", "@deepseek-ai/dsh"]);
        assert_eq!(args[3], "web");
    }

    #[test]
    fn a_dead_process_cannot_be_opened() {
        assert!(!wait_for_exit(0));
    }

    #[test]
    fn wait_for_exit_returns_when_the_child_ends() {
        let mut child = Command::new("cmd")
            .args(["/c", "ping -n 2 127.0.0.1 > nul"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a short-lived process");
        let started = Instant::now();
        assert!(wait_for_exit(child.id()), "a live child must be waitable");
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "wait returned before the process exited"
        );
        let _ = child.wait();
    }

    /// A PID plus start time identifies one process instance; the wait that
    /// watched it only reports when that same instance ends.
    #[test]
    fn a_process_key_survives_the_wait_it_watched() {
        let mut child = Command::new("cmd")
            .args(["/c", "ping -n 2 127.0.0.1 > nul"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a short-lived process");
        let key = process_key(child.id()).expect("the child is readable");
        assert_eq!(key.pid, child.id());
        assert_eq!(
            process_key(child.id()).map(|now| now.started),
            Some(key.started),
            "the start time is stable while the process lives"
        );
        assert!(wait_for_key(key), "the watched instance ended");
        // Windows keeps reporting the start time of an exited-but-unreaped
        // process, so the identity check must neither flip nor vanish.
        assert_eq!(
            process_key(key.pid).map(|now| now.started),
            Some(key.started),
            "an exited process keeps the start time it had"
        );
        let _ = child.wait();
    }

    /// PIDs are recycled: a made-up start time for a live PID must not be
    /// mistaken for the real process.
    #[test]
    fn a_recycled_pid_does_not_carry_the_old_start_time() {
        let real = process_key(std::process::id()).expect("this process is readable");
        let forged = ProcessKey {
            pid: real.pid,
            started: real.started.wrapping_add(1),
        };
        assert!(
            !is_same_process(forged),
            "a different start time is a different process"
        );
        assert!(is_same_process(real));
    }
}
