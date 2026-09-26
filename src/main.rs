//! Suwayomi 桌面壳（Tauri 2）：驻托盘，拉起 suwayomi-server，用系统 WebView
//! （Win WebView2 / Linux WebKitGTK / macOS WKWebView）打开 WebUI/设置窗口；
//! WebView 不可用时回退系统浏览器。无图形会话（Linux）降级为前台跑 server。
//! 发布布局（exe 同级）：bin/suwayomi-server(.exe) + bin/ext-runtime.jar +
//! webui/ + data/(工作数据) + db/(SQLite 库，与 data/ 分开) + extensions/。
//!
//! # 代码组织
//!
//! 按「纯内核 + 副作用外壳」分层，自上而下：
//!
//! 1. **配置与纯函数**（`Settings` / `SettingsPatch` / `server_env` / `webui_url` …）
//!    —— 不碰 I/O，可独立断言。
//! 2. **副作用薄壳**（`spawn_server` / `request_graceful_shutdown` / `tray_log` …）
//!    —— 只做 I/O，不含业务判断。
//! 3. **监督者 actor**（`Supervisor`）—— server 子进程的唯一所有者，外部只能发消息。
//! 4. **动作枚举 + 单一解释器**（`TrayAction` / `run_action`）—— 菜单项与执行分离。
//!
//! 共享可变状态只有一处：监督者线程里的 `ActorState`。托盘其余部分拿到的都是
//! 它的不可变快照 `Runtime`，因此不存在锁中毒，也不存在「改了一半」的状态。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sysinfo::{ProcessesToUpdate, System};
use tauri::menu::{IsMenuItem, Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};

// ─────────────────────────────── 常量 ───────────────────────────────

const DEFAULT_PORT: u16 = 8090;
const DATA_SUBDIRS: [&str; 3] = ["autobackup", "downloads", "local"];
const SERVER_PROC_NAMES: [&str; 2] = ["suwayomi-server.exe", "suwayomi-server"];

const WEBUI_WINDOW_LABEL: &str = "webui";
const SETTINGS_WINDOW_LABEL: &str = "settings";

/// 优雅关闭的轮询预算：500ms × 12 ≈ 6s，超时后强杀兜底
const POLL_EVERY: Duration = Duration::from_millis(500);
const STOP_POLL_TIMES: usize = 12;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const RESTART_READY_TIMEOUT: Duration = Duration::from_secs(25);
/// 向监督者提问的最长等待：覆盖「重启 server」最坏耗时（停 6s + 就绪 25s）
const SUPERVISOR_TIMEOUT: Duration = Duration::from_secs(35);

// ─────────────────────────── 1. 配置（纯数据） ───────────────────────────

/// 完整配置：**字段一律已归一化** —— 端口合法、`data_dir` 的空串已折叠为 `None`。
/// 因此「未设置工作目录」只有一种表示，读取方不必再写 `trim().is_empty()` 判断。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "SettingsPatch")]
struct Settings {
    server_port: u16,
    data_dir: Option<String>,
    open_web_ui_on_startup: bool,
    prefer_web_view: bool,
}

/// 用户实际写进文件的形状：字段可缺、可为空。反序列化后 fold 成 `Settings`。
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SettingsPatch {
    server_port: Option<u16>,
    #[serde(deserialize_with = "blank_to_none")]
    data_dir: Option<String>,
    open_web_ui_on_startup: Option<bool>,
    prefer_web_view: Option<bool>,
}

impl SettingsPatch {
    /// 纯：把「用户可能只写了一半」的配置折叠到默认值上。
    fn finish(self) -> Settings {
        Settings {
            // 非法端口回落到默认值，而不是 clamp 出一个用户没写过的数字
            server_port: self
                .server_port
                .filter(|p| (1..=65535).contains(p))
                .unwrap_or(DEFAULT_PORT),
            data_dir: non_empty(self.data_dir),
            open_web_ui_on_startup: self.open_web_ui_on_startup.unwrap_or(true),
            prefer_web_view: self.prefer_web_view.unwrap_or(true),
        }
    }
}

impl From<SettingsPatch> for Settings {
    fn from(patch: SettingsPatch) -> Self {
        patch.finish()
    }
}

impl Default for Settings {
    fn default() -> Self {
        SettingsPatch::default().into()
    }
}

/// 纯：去掉首尾空白，空串折叠为 `None`。
fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn blank_to_none<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(non_empty(Option::<String>::deserialize(d)?))
}

// ─────────────────────────── 2. 纯函数 ───────────────────────────

/// 发布布局根目录 = 本 exe 同级
fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 托盘设置文件：`<发布根>/settings/tray.json`。不放在数据目录里 —— 数据目录本身
/// 就是这个文件配的，放进去会自相矛盾。
fn settings_path() -> PathBuf {
    base_dir().join("settings").join("tray.json")
}

/// 工作数据目录解析：设置里自定义目录优先，否则 base/data
fn data_dir_of(s: &Settings) -> PathBuf {
    match s.data_dir.as_deref() {
        Some(d) => PathBuf::from(d),
        None => base_dir().join("data"),
    }
}

fn webui_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// WebView 内只允许 WebUI 自身源（127.0.0.1/localhost）的顶层导航
fn is_webui_origin(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
}

/// 纯：server 二进制的候选路径（按优先级）。不含托盘自身的 exe —— 否则 server
/// 缺失时会把托盘自己当 server 反复 spawn（fork 炸弹）。
fn server_bin_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Ok(p) = std::env::var("SUWAYOMI_BIN") {
        out.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in SERVER_PROC_NAMES {
                out.push(dir.join("bin").join(name));
            }
            for name in SERVER_PROC_NAMES {
                out.push(dir.join(name));
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in SERVER_PROC_NAMES {
                out.push(dir.join(name));
            }
        }
    }
    out
}

/// 纯：拉起 server 所需的环境变量（顺序无关，便于断言）。
fn server_env(data: &Path, port: u16, base: &Path, logs: &Path) -> Vec<(String, OsString)> {
    vec![
        ("SUWAYOMI_PORT".into(), port.to_string().into()),
        (
            "SUWAYOMI_EXTENSIONS_DIR".into(),
            base.join("extensions").into_os_string(),
        ),
        // 数据目录必须按解析结果显式传：server 自己的兜底是从 cwd 拼
        // `<cwd>/data/local`，而 cwd 已经设成 data，会解析成 `<data>/data/local`
        ("SUWAYOMI_DATA_DIR".into(), data.as_os_str().to_os_string()),
        (
            "SUWAYOMI_LOCAL_SOURCE_DIR".into(),
            data.join("local").into_os_string(),
        ),
        (
            "SUWAYOMI_LOGS_DIR".into(),
            logs.as_os_str().to_os_string(),
        ),
        (
            "SUWAYOMI_WEBUI_DIR".into(),
            base.join("webui").into_os_string(),
        ),
    ]
}

/// 高阶组合子：以固定间隔重试谓词。先立即判一次，再最多重试 `times - 1` 次。
fn poll_until(pred: impl Fn() -> bool, every: Duration, times: usize) -> bool {
    if pred() {
        return true;
    }
    (0..times.saturating_sub(1)).any(|_| {
        std::thread::sleep(every);
        pred()
    })
}

/// 进程名是否为 suwayomi-server
fn is_server_proc(name: &std::ffi::OsStr) -> bool {
    let n = name.to_string_lossy();
    SERVER_PROC_NAMES.contains(&n.as_ref())
}

// ─────────────────────────── 3. 错误类型 ───────────────────────────

#[derive(Debug)]
enum TrayError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Tauri(tauri::Error),
    Url(String),
    Icon(String),
    Menu(String),
    InvalidPort(u16),
    ServerNotFound,
    /// 监督者线程已退出（或忙到超时），无法应答
    SupervisorDown,
}

impl std::fmt::Display for TrayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O 失败: {e}"),
            Self::Json(e) => write!(f, "设置文件不是合法 JSON: {e}"),
            Self::Tauri(e) => write!(f, "{e}"),
            Self::Url(e) => write!(f, "URL 解析失败: {e}"),
            Self::Icon(e) => write!(f, "托盘图标解码失败: {e}"),
            Self::Menu(e) => write!(f, "托盘菜单创建失败: {e}"),
            Self::InvalidPort(p) => write!(f, "端口无效: {p}（应为 1-65535）"),
            Self::ServerNotFound => {
                write!(f, "server 启动失败（找不到 suwayomi-server 可执行文件）")
            }
            Self::SupervisorDown => write!(f, "server 管理线程无响应"),
        }
    }
}

impl std::error::Error for TrayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::Tauri(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TrayError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for TrayError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<tauri::Error> for TrayError {
    fn from(e: tauri::Error) -> Self {
        Self::Tauri(e)
    }
}

/// 前端按 `"保存失败: " + e` 直接拼接，所以序列化成纯字符串以保持契约。
impl Serialize for TrayError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

// ─────────────────────────── 4. 副作用薄壳 ───────────────────────────

/// 调试日志：写入 cache/logs/tray.log（release GUI 无控制台，eprintln 不可见）
fn tray_log(msg: &str) {
    let dir = base_dir().join("cache").join("logs");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("tray.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// 读设置。缺文件 / 解析失败都回落到默认值，但**留痕** —— 否则会表现成
/// "设置没生效"。
fn load_settings() -> Settings {
    match std::fs::read_to_string(settings_path()) {
        Ok(text) => match serde_json::from_str::<Settings>(&text) {
            Ok(s) => s,
            Err(e) => {
                tray_log(&format!(
                    "[tray] WARN: {} 解析失败（{e}），本次使用默认设置",
                    settings_path().display()
                ));
                Settings::default()
            }
        },
        Err(_) => Settings::default(), // 首次启动还没有这个文件
    }
}

fn save_settings_file(settings: &Settings) -> Result<(), TrayError> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    Ok(std::fs::write(path, json)?)
}

/// 定位 server 二进制：候选表里第一个真实存在的文件；找不到时把候选表打进日志。
fn find_server_bin() -> Option<PathBuf> {
    let candidates = server_bin_candidates();
    let found = candidates.iter().find(|p| p.is_file()).cloned();
    if found.is_none() {
        tray_log(&format!(
            "[tray] WARN: server binary not found (set SUWAYOMI_BIN or place \
             suwayomi-server next to this exe); candidates: {candidates:?}"
        ));
    }
    found
}

/// 端口是否接受连接：server 生命周期的判据（无需扫进程表）。
fn port_open(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// 是否已有 suwayomi-server 在运行：进程名匹配或端口可连，任一命中即视为已有。
fn server_running(port: u16) -> bool {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let by_name = sys.processes().values().any(|p| is_server_proc(p.name()));
    by_name || port_open(port)
}

/// 杀掉所有 suwayomi-server 进程（含外部启动的）
fn kill_server_processes() {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let ids: Vec<_> = sys
        .processes()
        .values()
        .filter(|p| is_server_proc(p.name()))
        .map(|p| p.pid())
        .collect();
    for id in ids {
        if let Some(p) = sys.process(id) {
            let _ = p.kill();
        }
    }
}

/// 请求 server 优雅关闭（POST loopback /api/v1/shutdown：停 postgres、杀 JVM 沙盒）
fn request_graceful_shutdown(port: u16) {
    tray_log(&format!("[tray] requesting graceful shutdown on port {port}"));
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return;
    };
    // 超时必须在 read 之前设置：否则 server 不响应时会永久阻塞调用线程
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!(
        "POST /api/v1/shutdown HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(req.as_bytes());
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
}

/// 优雅停 server：请求 shutdown → 轮询等待 → 超时强杀兜底 → 回收子进程句柄。
/// 强杀残留的 postgres 子进程由 server 下次启动自愈清理。
fn stop_server(port: u16, child: &mut Option<Child>) {
    if server_running(port) {
        request_graceful_shutdown(port);
        if poll_until(|| !server_running(port), POLL_EVERY, STOP_POLL_TIMES) {
            tray_log("[tray] server exited gracefully");
        } else {
            tray_log("[tray] graceful shutdown timed out; force-killing server");
            kill_server_processes();
        }
    }
    // 只回收句柄：Windows 父进程退出本就不杀子进程
    if let Some(mut c) = child.take() {
        let _ = c.wait();
    }
}

/// Poll TCP until the server accepts connections.
fn wait_ready(port: u16, timeout: Duration) -> bool {
    let times = (timeout.as_millis() / POLL_EVERY.as_millis().max(1)) as usize;
    poll_until(|| port_open(port), POLL_EVERY, times)
}

/// 数据子目录（autobackup/downloads/local）不存在时创建
fn ensure_data_dirs(data: &Path) -> Result<(), TrayError> {
    DATA_SUBDIRS
        .iter()
        .try_for_each(|d| std::fs::create_dir_all(data.join(d)))?;
    Ok(())
}

/// `inherit_stdio=true` 前台模式直接继承控制台；否则输出落 cache/logs/server.log
fn spawn_server(data: &Path, port: u16, inherit_stdio: bool) -> Result<Child, TrayError> {
    let bin = find_server_bin().ok_or(TrayError::ServerNotFound)?;
    let base = base_dir();
    let logs = base.join("cache").join("logs");
    std::fs::create_dir_all(&logs)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("server.log"))?;

    let mut command = Command::new(&bin);
    command.current_dir(data);
    for (k, v) in server_env(data, port, &base, &logs) {
        command.env(k, v);
    }
    if !inherit_stdio {
        command
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file));
    }

    let child = command.spawn()?;
    if inherit_stdio {
        eprintln!("[tray] spawned server {:?} on port {port}", bin);
    }
    Ok(child)
}

/// 编译期内联 PNG（8-bit RGBA/RGB）解码为 tauri Image（托盘图标用）。
/// 资源是编译期常量，写错本该是构建错误；这里至少不 panic，交由调用方降级。
fn decode_tray_icon(data: &[u8]) -> Result<tauri::image::Image<'static>, TrayError> {
    use png::{BitDepth, ColorType};

    let mut reader = png::Decoder::new(data)
        .read_info()
        .map_err(|e| TrayError::Icon(e.to_string()))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let (color_type, bit_depth, w, h) = {
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| TrayError::Icon(e.to_string()))?;
        (info.color_type, info.bit_depth, info.width, info.height)
    };
    let len = (w as usize) * (h as usize);

    let rgba = match (color_type, bit_depth) {
        (ColorType::Rgba, BitDepth::Eight) => buf[..len * 4].to_vec(),
        (ColorType::Rgb, BitDepth::Eight) => {
            let (chunks, _rest) = buf[..len * 3].as_chunks::<3>();
            let mut out = Vec::with_capacity(len * 4);
            for [r, g, b] in chunks {
                out.extend_from_slice(&[*r, *g, *b, 255]);
            }
            out
        }
        other => return Err(TrayError::Icon(format!("unsupported png format {other:?}"))),
    };
    Ok(tauri::image::Image::new_owned(rgba, w, h))
}

// ─────────────────────────── 5. 监督者 actor ───────────────────────────

/// 监督者线程私有的状态：server 子进程的唯一所有者。
struct ActorState {
    settings: Settings,
    data: PathBuf,
    child: Option<Child>,
    /// false = 托盘退出后不再关停 server（「隐藏托盘」，或已自行发起关闭）
    attached: bool,
}

/// 对外暴露的不可变快照。
#[derive(Debug, Clone)]
struct Runtime {
    settings: Settings,
    data: PathBuf,
}

impl Runtime {
    fn port(&self) -> u16 {
        self.settings.server_port
    }
}

enum SupervisorMsg {
    /// 落盘新配置，并按新配置（重新）拉起 server
    Apply {
        settings: Settings,
        reply: Sender<Result<(), TrayError>>,
    },
    /// 按当前配置重启
    Restart { reply: Sender<Result<(), TrayError>> },
    /// 托盘退出后不再关停 server
    Detach { reply: Sender<()> },
    /// 取一份运行时快照
    Snapshot { reply: Sender<Runtime> },
    /// 收尾：按 attached 决定是否发关闭请求，然后线程退出
    Shutdown { reply: Sender<()> },
}

/// 监督者的句柄：只有消息通道和一个 join handle，状态本身拿不到。
#[derive(Clone)]
struct Supervisor {
    tx: Arc<Sender<SupervisorMsg>>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Supervisor {
    fn spawn(state: ActorState) -> Supervisor {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || supervisor_loop(state, rx));
        Supervisor {
            tx: Arc::new(tx),
            thread: Arc::new(Mutex::new(Some(thread))),
        }
    }

    /// 统一的「提问—应答」：监督者忙或已退出时返回 `SupervisorDown`，而不是死锁。
    fn ask<T>(&self, msg: impl FnOnce(Sender<T>) -> SupervisorMsg) -> Result<T, TrayError> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(msg(reply))
            .map_err(|_| TrayError::SupervisorDown)?;
        rx.recv_timeout(SUPERVISOR_TIMEOUT)
            .map_err(|_| TrayError::SupervisorDown)
    }

    fn apply(&self, settings: Settings) -> Result<(), TrayError> {
        self.ask(|reply| SupervisorMsg::Apply { settings, reply })?
    }

    fn restart(&self) -> Result<(), TrayError> {
        self.ask(|reply| SupervisorMsg::Restart { reply })?
    }

    fn detach(&self) -> Result<(), TrayError> {
        self.ask(|reply| SupervisorMsg::Detach { reply })
    }

    fn runtime(&self) -> Option<Runtime> {
        self.ask(|reply| SupervisorMsg::Snapshot { reply }).ok()
    }

    /// 退出收尾：通知监督线程停止并等待其退出（唯一一次 join）。
    fn shutdown(&self) {
        let _ = self.ask(|reply| SupervisorMsg::Shutdown { reply });
        let handle = self.thread.lock().ok().and_then(|mut guard| guard.take());
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

fn supervisor_loop(state: ActorState, rx: Receiver<SupervisorMsg>) {
    let mut state = state;

    for msg in rx {
        match msg {
            SupervisorMsg::Apply { settings, reply } => {
                let _ = reply.send(apply_settings(&mut state, settings));
            }
            SupervisorMsg::Restart { reply } => {
                let _ = reply.send(restart_server(&mut state));
            }
            SupervisorMsg::Detach { reply } => {
                state.attached = false;
                let _ = reply.send(());
            }
            SupervisorMsg::Snapshot { reply } => {
                let _ = reply.send(Runtime {
                    settings: state.settings.clone(),
                    data: state.data.clone(),
                });
            }
            SupervisorMsg::Shutdown { reply } => {
                let _ = reply.send(());
                break;
            }
        }
    }

    // 线程退出（收到 Shutdown，或所有句柄被丢弃）：仍托管则发一次非阻塞的关闭请求
    if state.attached {
        request_graceful_shutdown(state.settings.server_port);
    }
    drop(state.child);
}

/// 落盘 → 建目录 → 停旧的 → 起新的。内存状态紧跟着磁盘提交，中途失败也不分叉。
fn apply_settings(state: &mut ActorState, next: Settings) -> Result<(), TrayError> {
    save_settings_file(&next)?;
    let next_data = data_dir_of(&next);
    ensure_data_dirs(&next_data)?;
    std::fs::create_dir_all(&next_data)?;

    stop_server(state.settings.server_port, &mut state.child);

    let child = spawn_server(&next_data, next.server_port, false);
    // 配置已落盘，内存立即跟进 —— 状态与磁盘保持一致，即便启动失败
    state.settings = next;
    state.data = next_data;

    match child {
        Ok(c) => {
            state.child = Some(c);
            let _ = wait_ready(state.settings.server_port, READY_TIMEOUT);
            Ok(())
        }
        Err(e) => {
            tray_log(&format!("[tray] restart after settings change failed: {e}"));
            Err(e)
        }
    }
}

/// 按当前配置重启：先优雅停（内部已等进程退出），再重新拉起。
fn restart_server(state: &mut ActorState) -> Result<(), TrayError> {
    let port = state.settings.server_port;
    let data = state.data.clone();

    tray_log("[tray] restart: stopping running server");
    stop_server(port, &mut state.child);
    // 进程已退出，但端口释放可能还有延迟
    std::thread::sleep(Duration::from_secs(2));

    match spawn_server(&data, port, false) {
        Ok(c) => {
            state.child = Some(c);
            let _ = wait_ready(port, RESTART_READY_TIMEOUT);
            Ok(())
        }
        Err(e) => {
            tray_log(&format!("[tray] restart failed: {e}"));
            Err(e)
        }
    }
}

// ─────────────────────────── 6. 窗口 ───────────────────────────

/// 设置页前端：编译期内联，经自定义协议提供（纯 cargo build 不嵌 frontendDist）
const SETTINGS_HTML: &str = include_str!("../frontend/index.html");

fn build_settings_window(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow, TrayError> {
    let url = tauri::Url::parse("settings://localhost/index.html")
        .map_err(|e| TrayError::Url(e.to_string()))?;
    WebviewWindowBuilder::new(app, SETTINGS_WINDOW_LABEL, WebviewUrl::CustomProtocol(url))
        .title("Suwayomi 设置")
        .inner_size(480.0, 380.0)
        .resizable(false)
        .theme(Some(tauri::Theme::Dark))
        // 以不可见状态创建——启动时完全不闪现，托盘「设置」菜单才 show()
        .visible(false)
        .build()
        .map_err(TrayError::Tauri)
}

fn show_settings_window(app: &tauri::AppHandle) {
    match app.get_webview_window(SETTINGS_WINDOW_LABEL) {
        Some(w) => {
            let _ = w.show();
            let _ = w.set_focus();
        }
        // 无 WebView 引擎（精简系统）：设置窗口不可用
        None => tray_log("[tray] settings window not available (no system webview)"),
    }
}

/// 打开/聚焦 WebUI 窗口（label "webui"，已存在→show+focus，销毁后重建）。
/// 返回 false = 系统 WebView 不可用，调用方应回退系统浏览器。
fn open_webui_window(app: &tauri::AppHandle, port: u16) -> bool {
    if let Some(w) = app.get_webview_window(WEBUI_WINDOW_LABEL) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        tray_log("[tray] webui window exists; focusing existing window");
        return true;
    }

    let Ok(url) = tauri::Url::parse(&webui_url(port)) else {
        tray_log(&format!("[tray] invalid webui url for port {port}"));
        return false;
    };

    match WebviewWindowBuilder::new(app, WEBUI_WINDOW_LABEL, WebviewUrl::External(url))
        .title("Suwayomi")
        .inner_size(1280.0, 860.0)
        // WebUI 里的外部链接（关于/文档、追踪器授权页等）一律交系统浏览器：拦截顶层导航
        // 与 target=_blank/window.open 新窗请求，不在 WebView 里打开
        .on_navigation(move |url| {
            if is_webui_origin(url) {
                return true;
            }
            tray_log(&format!("[tray] external link -> browser: {url}"));
            let _ = open::that(url.to_string());
            false
        })
        .on_new_window(move |url, _features| {
            tray_log(&format!("[tray] external link (new window) -> browser: {url}"));
            let _ = open::that(url.to_string());
            tauri::webview::NewWindowResponse::Deny
        })
        .build()
    {
        Ok(w) => {
            // 不 set_icon：标题栏与任务栏共用 WM_SETICON，set 后任务栏也会变
            let _ = w.show();
            let _ = w.set_focus();
            tray_log(&format!("[tray] opened webui window: {}", webui_url(port)));
            true
        }
        Err(e) => {
            tray_log(&format!(
                "[tray] system webview unavailable ({e}); falling back to system browser"
            ));
            false
        }
    }
}

/// 打开 WebUI：设置开启且 WebView 窗口可用 → 窗口，否则系统浏览器
fn launch_webui(app: &tauri::AppHandle, rt: &Runtime) {
    if rt.settings.prefer_web_view && open_webui_window(app, rt.port()) {
        return;
    }
    let url = webui_url(rt.port());
    tray_log(&format!("[tray] opening webui in system browser: {url}"));
    let _ = open::that(url);
}

// ────────────────────── 7. 菜单动作：枚举 + 解释器 ──────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayAction {
    Start,
    OpenWebUi,
    OpenData,
    Settings,
    HideTray,
    Quit,
}

impl TrayAction {
    const ALL: [TrayAction; 6] = [
        TrayAction::Start,
        TrayAction::OpenWebUi,
        TrayAction::OpenData,
        TrayAction::Settings,
        TrayAction::HideTray,
        TrayAction::Quit,
    ];

    /// 菜单项 id 的唯一真源：构建与分发共用，拼错当场暴露
    fn id(self) -> &'static str {
        match self {
            Self::Start => "start_suwayomi",
            Self::OpenWebUi => "open_webui",
            Self::OpenData => "open_data",
            Self::Settings => "settings",
            Self::HideTray => "hide_tray",
            Self::Quit => "quit",
        }
    }

    /// 静态菜单文字；`Start` 的文案随 server 状态变化，见 `start_label`
    fn label(self) -> &'static str {
        match self {
            Self::Start => "启动 Suwayomi",
            Self::OpenWebUi => "打开 WebUI",
            Self::OpenData => "打开数据目录",
            Self::Settings => "设置",
            Self::HideTray => "隐藏托盘",
            Self::Quit => "退出",
        }
    }
}

impl FromStr for TrayAction {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.iter().copied().find(|a| a.id() == s).ok_or(())
    }
}

/// 「启动/重启」项始终可点：运行中 = 重启，未运行 = 启动
fn start_label(running: bool) -> &'static str {
    if running {
        "重启 Suwayomi"
    } else {
        "启动 Suwayomi"
    }
}

fn build_menu(app: &tauri::AppHandle, running: bool) -> Result<Menu<tauri::Wry>, TrayError> {
    let items = TrayAction::ALL
        .iter()
        .map(|action| {
            let text = match action {
                TrayAction::Start => start_label(running),
                other => other.label(),
            };
            MenuItem::with_id(app, action.id(), text, true, None::<&str>)
                .map_err(|e| TrayError::Menu(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let refs = items
        .iter()
        .map(|i| i as &dyn IsMenuItem<tauri::Wry>)
        .collect::<Vec<_>>();
    Menu::with_items(app, &refs).map_err(|e| TrayError::Menu(e.to_string()))
}

/// 单一解释器：所有托盘动作只在这里被执行（穷尽匹配，加菜单项时编译器强制处理）。
fn run_action(action: TrayAction, app: &tauri::AppHandle, supervisor: &Supervisor) {
    match action {
        TrayAction::Start => {
            if let Err(e) = supervisor.restart() {
                tray_log(&format!("[tray] start/restart server failed: {e}"));
            }
        }
        TrayAction::OpenWebUi => {
            if let Some(rt) = supervisor.runtime() {
                launch_webui(app, &rt);
            }
        }
        TrayAction::OpenData => {
            if let Some(rt) = supervisor.runtime() {
                let _ = open::that(&rt.data);
            }
        }
        TrayAction::Settings => show_settings_window(app),
        TrayAction::HideTray => {
            // 托盘退出、server 保持后台运行
            tray_log("[tray] hide_tray: keeping server running, exiting tray");
            let _ = supervisor.detach();
            app.exit(0);
        }
        TrayAction::Quit => {
            // 托盘立即退出：只发优雅关闭请求，server 后台自行收尾。
            // 已自行发起，标记 detach 以免退出收尾时重复请求。
            if let Some(rt) = supervisor.runtime() {
                request_graceful_shutdown(rt.port());
            }
            let _ = supervisor.detach();
            app.exit(0);
        }
    }
}

// ─────────────────────────── 8. Tauri 命令 ───────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    server_port: u16,
    /// 当前实际生效的工作目录路径
    data_dir: String,
    /// 设置里的自定义目录（空 = 默认 base/data）
    data_dir_override: String,
    open_web_ui_on_startup: bool,
    prefer_web_view: bool,
    web_ui_url: String,
}

impl SettingsView {
    /// 纯：运行时快照 → 前端读模型（不再二次读文件，单一真源）
    fn of(rt: &Runtime) -> SettingsView {
        SettingsView {
            server_port: rt.settings.server_port,
            data_dir: rt.data.display().to_string(),
            data_dir_override: rt.settings.data_dir.clone().unwrap_or_default(),
            open_web_ui_on_startup: rt.settings.open_web_ui_on_startup,
            prefer_web_view: rt.settings.prefer_web_view,
            web_ui_url: webui_url(rt.settings.server_port),
        }
    }
}

#[tauri::command]
fn get_settings(supervisor: State<Supervisor>) -> Result<SettingsView, TrayError> {
    let rt = supervisor.runtime().ok_or(TrayError::SupervisorDown)?;
    Ok(SettingsView::of(&rt))
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    supervisor: State<Supervisor>,
    server_port: u16,
    data_dir: Option<String>,
    open_web_ui_on_startup: bool,
    prefer_web_view: bool,
) -> Result<String, TrayError> {
    // 校验：非法端口直接拒绝，而不是悄悄 clamp 出一个用户没填过的数字
    if !(1..=65535).contains(&server_port) {
        return Err(TrayError::InvalidPort(server_port));
    }
    let next: Settings = SettingsPatch {
        server_port: Some(server_port),
        data_dir: Some(data_dir.unwrap_or_default()),
        open_web_ui_on_startup: Some(open_web_ui_on_startup),
        prefer_web_view: Some(prefer_web_view),
    }
    .into();

    // 关掉旧端口的 WebUI 窗口，再按新配置重启
    if let Some(w) = app.get_webview_window(WEBUI_WINDOW_LABEL) {
        let _ = w.close();
    }
    supervisor.apply(next.clone())?;
    Ok(webui_url(next.server_port))
}

#[tauri::command]
fn open_data_dir(supervisor: State<Supervisor>) -> Result<(), TrayError> {
    let rt = supervisor.runtime().ok_or(TrayError::SupervisorDown)?;
    open::that(&rt.data).map_err(|e| TrayError::Io(std::io::Error::other(e)))
}

#[tauri::command]
fn webui_url_cmd(supervisor: State<Supervisor>) -> Result<String, TrayError> {
    let rt = supervisor.runtime().ok_or(TrayError::SupervisorDown)?;
    Ok(webui_url(rt.port()))
}

// ─────────────────────────── 9. 启动 ───────────────────────────

/// Linux 是否有图形会话（DISPLAY/WAYLAND_DISPLAY）。无会话时直接跑 GTK 会
/// panic，启动前探测并降级为前台 server 模式。
#[cfg(target_os = "linux")]
fn has_graphical_session() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

/// 无图形会话降级：沿用 settings 的端口/数据目录前台跑 server（等价于直接
/// 跑 bin/suwayomi-server），不返回。
#[cfg(target_os = "linux")]
fn run_server_foreground() -> ! {
    let settings = load_settings();
    let port = settings.server_port;
    let data = data_dir_of(&settings);
    if let Err(e) = ensure_data_dirs(&data) {
        eprintln!("failed to prepare data dir: {e}");
        std::process::exit(1);
    }

    if server_running(port) {
        eprintln!("suwayomi-server already running on port {port}; nothing to do");
        std::process::exit(1);
    }

    let mut child = match spawn_server(&data, port, true) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{e}.\nPlace it in ./bin/ or set SUWAYOMI_BIN=/path/to/suwayomi-server"
            );
            std::process::exit(1);
        }
    };

    if !wait_ready(port, READY_TIMEOUT) {
        tray_log("[tray] foreground server did not become ready within 20s");
    }
    eprintln!(
        "suwayomi-server ready on {} (Ctrl-C to stop)",
        webui_url(port)
    );

    match child.wait() {
        Ok(status) => std::process::exit(status.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("failed to wait for server: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "linux")]
fn maybe_run_headless() {
    if !has_graphical_session() {
        run_server_foreground();
    }
}

#[cfg(not(target_os = "linux"))]
fn maybe_run_headless() {}

/// 启动引导：读设置 → 解析数据目录 → 探测是否已有 server → 拉起 → 交给监督者。
fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // macOS：托盘应用按「菜单栏附件」对待（NSApplicationActivationPolicyAccessory），
    // 否则非 bundle 的裸可执行文件会在 Dock 里占一个图标、并且抢激活。窗口仍能正常
    // 显示/聚焦，只是不进 Dock、不出现在 Cmd-Tab 列表。Windows/Linux 没有这个概念，
    // 该 API 本身也是 macOS 专属（`#[cfg(target_os = "macos")]`）。
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    let settings = load_settings();
    let port = settings.server_port;
    let data = data_dir_of(&settings);
    ensure_data_dirs(&data)?;

    // 探测只做一次：菜单文案与是否拉起 server 必须基于同一个答案
    let already_running = server_running(port);
    tray_log(&format!("[tray] setup: server_running={already_running}"));

    let mut child = None;
    if already_running {
        tray_log("[tray] server already running; not starting another");
    } else {
        match spawn_server(&data, port, false) {
            Ok(c) => {
                child = Some(c);
                let _ = wait_ready(port, READY_TIMEOUT);
            }
            Err(e) => tray_log(&format!(
                "[tray] WARN: server not started ({e}); set SUWAYOMI_BIN or place \
                 suwayomi-server next to this exe"
            )),
        }
    }
    let started = child.is_some();

    // 设置窗口：不可见创建（托盘菜单唤起）；WebView 不可用仅告警不中断
    if let Err(e) = build_settings_window(app.handle()) {
        tray_log(&format!(
            "[tray] settings window unavailable (no system webview?): {e}"
        ));
    }

    let supervisor = Supervisor::spawn(ActorState {
        settings: settings.clone(),
        data: data.clone(),
        child,
        attached: true,
    });
    app.manage(supervisor.clone());

    let menu = build_menu(app.handle(), already_running || started)?;

    // 托盘小图标用专用 tray.png（「坐」放大版，小尺寸可读）；不用
    // default_window_icon/exe ICO（缩放会糊）。窗口/任务栏仍走 exe ICO。
    let mut tray = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("Suwayomi")
        .show_menu_on_left_click(false);
    match decode_tray_icon(include_bytes!("../icons/tray.png")) {
        Ok(icon) => tray = tray.icon(icon),
        Err(e) => tray_log(&format!("[tray] tray icon unavailable: {e}")),
    }

    // 图标已注册进 app 的资源表，`_tray` 离开作用域不会把它撤掉
    let _tray = tray
        // 单击托盘图标 = 打开 WebUI（与菜单「打开 WebUI」行为一致）
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(supervisor) = app.try_state::<Supervisor>() {
                    if let Some(rt) = supervisor.runtime() {
                        launch_webui(app, &rt);
                    }
                }
            }
        })
        .on_menu_event(|app, event| {
            // 解析而非校验：未知 id 直接记日志返回，不再有 `_ => {}` 静默吞掉拼写错误
            let Ok(action) = TrayAction::from_str(event.id.as_ref()) else {
                tray_log(&format!("[tray] unknown menu id: {}", event.id.as_ref()));
                return;
            };
            if let Some(supervisor) = app.try_state::<Supervisor>() {
                run_action(action, app, &supervisor);
            }
        })
        .build(app)?;

    // 启动即打开 WebUI（设置可关）：本托盘拉起或 server 已在运行都要开
    if settings.open_web_ui_on_startup && (started || already_running) {
        launch_webui(app.handle(), &Runtime { settings, data });
    }
    Ok(())
}

/// settings 关闭 = 隐藏（驻托盘）；webui 关闭 = 真销毁（下次打开重建）
fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != SETTINGS_WINDOW_LABEL {
        return;
    }
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        let _ = window.hide();
        api.prevent_close();
    }
}

fn on_run_event(app: &tauri::AppHandle, event: RunEvent) {
    if !matches!(event, RunEvent::Exit) {
        return;
    }
    // 退出收尾：显式、可预期（取代原先藏在 `Drop` 里的副作用）
    if let Some(supervisor) = app.try_state::<Supervisor>() {
        supervisor.shutdown();
    }
}

fn main() {
    // 必须早于 tauri::Builder：无图形会话时 GTK 初始化会直接 panic，来不及兜底
    maybe_run_headless();

    if let Err(e) = try_main() {
        tray_log(&format!("[tray] fatal: {e}"));
        eprintln!("[tray] fatal: {e}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<(), TrayError> {
    let app = tauri::Builder::default()
        // 单实例：重复启动时第二实例直接退出，聚焦已有窗口
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray_log("[tray] single-instance: second launch detected, focusing existing window");
            if let Some(w) = app.get_webview_window(WEBUI_WINDOW_LABEL) {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            } else if let Some(w) = app.get_webview_window(SETTINGS_WINDOW_LABEL) {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            open_data_dir,
            webui_url_cmd
        ])
        .register_uri_scheme_protocol("settings", |_ctx, _req| {
            tauri::http::Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(SETTINGS_HTML.as_bytes().to_vec())
                .expect("build settings page response")
        })
        .setup(setup)
        .on_window_event(on_window_event)
        .build(tauri::generate_context!())?;

    app.run(on_run_event);
    Ok(())
}

// ─────────────────────────── 10. 纯内核的单元测试 ───────────────────────────
//
// 分层的收益就在这里：这些断言不需要起 GUI、不需要真 server、不需要临时目录。

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(json: &str) -> Settings {
        serde_json::from_str::<Settings>(json).expect("parse settings")
    }

    #[test]
    fn missing_fields_fold_to_defaults() {
        let s = settings("{}");
        assert_eq!(s.server_port, DEFAULT_PORT);
        assert_eq!(s.data_dir, None);
        assert!(s.open_web_ui_on_startup);
        assert!(s.prefer_web_view);
    }

    #[test]
    fn partial_file_keeps_unspecified_defaults() {
        let s = settings(r#"{"serverPort": 9000}"#);
        assert_eq!(s.server_port, 9000);
        assert_eq!(s.data_dir, None);
        assert!(s.prefer_web_view);
    }

    /// 「未设置工作目录」只有一种表示：空串与空白都折叠为 None
    #[test]
    fn blank_data_dir_is_none() {
        assert_eq!(settings(r#"{"dataDir": ""}"#).data_dir, None);
        assert_eq!(settings(r#"{"dataDir": "   "}"#).data_dir, None);
        assert_eq!(
            settings(r#"{"dataDir": " /srv/manga "}"#)
                .data_dir
                .as_deref(),
            Some("/srv/manga")
        );
    }

    #[test]
    fn out_of_range_port_falls_back_to_default() {
        // 0 是合法 u16 但不是合法端口 → 字段级回落到默认
        assert_eq!(settings(r#"{"serverPort": 0}"#).server_port, DEFAULT_PORT);
        assert_eq!(settings(r#"{"serverPort": 65535}"#).server_port, 65535);
        // 超出 u16 连反序列化都过不去 → 整个文件判为损坏，回落全套默认（与旧行为一致）
        assert!(serde_json::from_str::<Settings>(r#"{"serverPort": 70000}"#).is_err());
    }

    #[test]
    fn default_settings_comes_from_the_empty_patch() {
        assert_eq!(Settings::default().server_port, DEFAULT_PORT);
        assert_eq!(Settings::default().data_dir, None);
    }

    #[test]
    fn data_dir_defaults_under_base() {
        assert_eq!(data_dir_of(&Settings::default()), base_dir().join("data"));
        assert_eq!(
            data_dir_of(&settings(r#"{"dataDir": "/srv/manga"}"#)),
            PathBuf::from("/srv/manga")
        );
    }

    #[test]
    fn webui_url_is_loopback() {
        assert_eq!(webui_url(8090), "http://127.0.0.1:8090");
    }

    #[test]
    fn only_local_origins_are_allowed_in_the_webview() {
        let localhost = tauri::Url::parse("http://127.0.0.1:8090/x").unwrap();
        let named = tauri::Url::parse("http://localhost:8090").unwrap();
        let external = tauri::Url::parse("https://example.com").unwrap();
        assert!(is_webui_origin(&localhost));
        assert!(is_webui_origin(&named));
        assert!(!is_webui_origin(&external));
    }

    #[test]
    fn server_env_covers_every_dir_the_server_needs() {
        let env = server_env(
            Path::new("/data"),
            1234,
            Path::new("/base"),
            Path::new("/logs"),
        );
        let keys = env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>();
        for k in [
            "SUWAYOMI_PORT",
            "SUWAYOMI_DATA_DIR",
            "SUWAYOMI_LOCAL_SOURCE_DIR",
            "SUWAYOMI_WEBUI_DIR",
            "SUWAYOMI_EXTENSIONS_DIR",
            "SUWAYOMI_LOGS_DIR",
        ] {
            assert!(keys.contains(&k), "missing {k}");
        }
        let port = env
            .iter()
            .find(|(k, _)| k == "SUWAYOMI_PORT")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(port, "1234");
    }

    /// 候选表里绝不能出现托盘自己：server 缺失时会变成 fork 炸弹
    #[test]
    fn server_bin_candidates_only_contain_server_names() {
        for c in server_bin_candidates() {
            let name = c.file_name().unwrap().to_string_lossy();
            assert!(
                SERVER_PROC_NAMES.contains(&name.as_ref()),
                "unexpected server candidate: {c:?}"
            );
        }
    }

    #[test]
    fn poll_until_short_circuits_and_gives_up() {
        assert!(poll_until(|| true, Duration::from_millis(1), 5));
        assert!(!poll_until(|| false, Duration::from_millis(1), 3));

        // `poll_until` 只接受 `Fn`（谓词必须无副作用），所以计数要用 Cell
        let calls = std::cell::Cell::new(0);
        assert!(poll_until(
            || {
                calls.set(calls.get() + 1);
                calls.get() == 2
            },
            Duration::from_millis(1),
            5
        ));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn every_action_round_trips_through_its_id() {
        for action in TrayAction::ALL {
            assert_eq!(TrayAction::from_str(action.id()), Ok(action));
        }
        assert!(TrayAction::from_str("nope").is_err());

        let mut ids = TrayAction::ALL.iter().map(|a| a.id()).collect::<Vec<_>>();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate menu id");
    }

    #[test]
    fn non_empty_collapses_blank() {
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(Some("  ".into())), None);
        assert_eq!(non_empty(Some(" x ".into())).as_deref(), Some("x"));
    }

    #[test]
    fn settings_view_maps_runtime_to_the_frontend_read_model() {
        let rt = Runtime {
            settings: settings(r#"{"dataDir":"/srv","preferWebView":false}"#),
            data: PathBuf::from("/srv"),
        };
        let view = SettingsView::of(&rt);
        assert_eq!(view.data_dir, "/srv");
        assert_eq!(view.data_dir_override, "/srv");
        assert!(!view.prefer_web_view);
        assert_eq!(view.web_ui_url, webui_url(DEFAULT_PORT));
    }
}
