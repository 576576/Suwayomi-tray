//! Suwayomi 桌面壳（Tauri 2）：驻托盘，拉起 suwayomi-server，用系统 WebView
//! （Win WebView2 / Linux WebKitGTK / macOS WKWebView）打开 WebUI/设置窗口；
//! WebView 不可用时回退系统浏览器。无图形会话（Linux）降级为前台跑 server。
//! 发布布局（exe 同级）：bin/suwayomi-server(.exe) + bin/ext-runtime.jar +
//! webui/ + appdata/(程序状态：cache/db/logs/settings/extensions) + data/(工作数据)。
//! `webui/` 恒在包内（只读资源），另外两个根可被环境变量或安装包预置外指。
//!
//! # 代码组织
//!
//! 按「纯内核 + 副作用外壳」分层，自上而下：
//!
//! 1. **配置与纯函数**（`Settings` / `SettingsPatch` / `resolve_ports` / `server_env`
//!    / `webui_url` …）—— 不碰 I/O，可独立断言。
//! 2. **副作用薄壳**（`spawn_server` / `request_graceful_shutdown` / `Logger` …）
//!    —— 只做 I/O，不含业务判断。
//! 3. **监督者 actor**（`Supervisor`）—— server 子进程的唯一所有者，外部只能发消息。
//! 4. **动作枚举 + 单一解释器**（`TrayAction` / `run_action`）—— 菜单项与执行分离。
//!
//! 共享可变状态只有一处：监督者线程里的 `ActorState`。托盘其余部分拿到的都是
//! 它的不可变快照 `Runtime`，因此不存在锁中毒，也不存在「改了一半」的状态。
//!
//! # 错误处理
//! 业务结果一律不得丢：server 起不来、配置写不进，都必须留痕。能传播的走
//! `Result` + `?`；传播不上去的走 [`best_effort`] 写日志，不静默吞掉。
//! 裸 `let _ =` 仅保留在三处并带 `// INTENTIONAL:` 说明理由。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sysinfo::{ProcessesToUpdate, System};
use tauri::menu::{IsMenuItem, Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};

// 编译期生成：build.rs 解码 icons/tray.png 写入 OUT_DIR/tray_icon.rs
mod tray_icon {
    include!(concat!(env!("OUT_DIR"), "/tray_icon.rs"));
}

// ─────────────────────────────── 常量 ───────────────────────────────

/// 没在设置里指定端口时用的默认值，与 server 的默认值（`ServerConfig::default`）一致。
/// Windows 上它落在 Hyper-V 动态保留区（4501-4900，bind 报 10013）里，所以自动档
/// 一律要嗅探（见 `resolve_ports`）。
const DEFAULT_PORT: u16 = 4567;
/// 沙盒（ext-runtime JVM）端口的嗅探起点：紧邻 server 默认端口的下一格。
/// 两者同处 Hyper-V 保留区，所以这一格同样要靠嗅探（见 `resolve_ports`）。
const SANDBOX_PORT_DEFAULT: u16 = 4568;
/// 端口嗅探的最大步数：只用来躲开被占 / 被系统保留的端口，不做全端口扫描。
const PORT_SNIFF_TRIES: u16 = 32;
const DATA_SUBDIRS: [&str; 3] = ["autobackup", "downloads", "local"];

/// exe 同级的 appdata 根目录名。server 侧 `SUWAYOMI_APPDATA_DIR` 的兜底同名。
const APPDATA_DIR_NAME: &str = "appdata";
/// 托盘设置文件在 appdata 根下的相对路径 —— 安装包的预置组件按同一个相对路径投放，
/// 两处必须一致。
const TRAY_SETTINGS_REL: &str = "settings/tray.json";

/// appdata 下 server 会用到的子目录（与 Rust 侧 `AppPaths` 的子路径一一对应）。
/// 启动前先建出来，省得首启时 server 边跑边造。
const APPDATA_SUBDIRS: [&str; 6] = [
    "cache",
    "logs",
    "db",
    "settings",
    "extensions/apk",
    "extensions/bin",
];

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
    /// 端口：`None` = 没指定，由托盘在启动前嗅探（默认 4567，被占则向上顺延）；
    /// `Some` = 用户写死的端口，必须照用 —— 他可能按这个端口配了防火墙/端口转发，
    /// 悄悄换掉等于把服务藏到别处。
    server_port: Option<u16>,
    /// WebUI 打开的目标地址（host、host:port 或完整 URL）。
    /// `None` = 本机回环 + 实际端口，与没有这个设置时完全一样。
    server_address: Option<String>,
    data_dir: Option<String>,
    open_web_ui_on_startup: bool,
    prefer_web_view: bool,
}

/// 用户实际写进文件的形状：字段可缺、可为空。反序列化后 fold 成 `Settings`。
///
/// `deny_unknown_fields`：这是**用户手写**的配置文件，拼错的键（`serverport`）
/// 不该被 serde 静默丢掉。拒绝未知键后，拼错会变成解析失败 → `load_settings`
/// 记 WARN 并回落默认值 —— 至少留痕，而不是悄悄用错端口。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct SettingsPatch {
    server_port: Option<u16>,
    #[serde(deserialize_with = "blank_to_none")]
    server_address: Option<String>,
    #[serde(deserialize_with = "blank_to_none")]
    data_dir: Option<String>,
    open_web_ui_on_startup: Option<bool>,
    prefer_web_view: Option<bool>,
}

impl SettingsPatch {
    /// 纯：把「用户可能只写了一半」的配置折叠到默认值上。
    fn finish(self) -> Settings {
        Settings {
            // 非法端口当作没写，而不是 clamp 出一个用户没写过的数字
            server_port: self.server_port.filter(|p| (1..=65535).contains(p)),
            server_address: non_empty(self.server_address),
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

/// 安装包预置的两个根。per-machine 的 msi / setup.exe 随包落一份
/// `<安装根>\appdata\settings\tray.json`（组件条件
/// `ALLUSERS = 1 OR (ALLUSERS = 2 AND NOT MSIINSTALLPERUSER)`），把根指到用户目录 ——
/// 那种安装落在 `Program Files`，普通用户对它没有写权限，首次启动会直接失败。
///
/// 两个值都是**默认层**：设置里显式写过的压过它们。路径写环境变量、不写死绝对路径：
/// per-machine 安装由管理员执行，写死就等于把数据落到管理员的用户目录。
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PresetSettings {
    /// appdata 根（程序自身状态：缓存 / 库 / 日志 / 设置 / 扩展）
    appdata_dir: Option<String>,
    /// 工作数据根（下载 / 本地图源 / 自动备份都在它之下）
    data_dir: Option<String>,
}

// ─────────────────────────── 2. 纯函数 ───────────────────────────

/// 发布布局根目录 = 本 exe 同级
fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 纯：展开字符串里的 `%VAR%`（`lookup` 注入，便于断言）。查不到的变量名、空名字
/// （`%%`）、以及落单的 `%` 一律返回 `None`，由调用方按「这一项没设置」处理 ——
/// 照原样返回的话会真的建出一个叫 `%FOO%` 的目录。
fn expand_vars(s: &str, lookup: impl Fn(&str) -> Option<OsString>) -> Option<OsString> {
    let mut out = OsString::new();
    let mut rest = s;
    while let Some(open) = rest.find('%') {
        out.push(&rest[..open]);
        let after = &rest[open + 1..];
        let name = &after[..after.find('%')?];
        if name.is_empty() {
            return None;
        }
        out.push(lookup(name)?);
        rest = &after[name.len() + 1..];
    }
    out.push(rest);
    Some(out)
}

/// 用进程环境展开 `%VAR%`。Windows 上变量名不区分大小写，这一点由 `var_os` 保证。
fn expand_env_path(s: &str) -> Option<PathBuf> {
    expand_vars(s, |n| std::env::var_os(n)).map(PathBuf::from)
}

/// appdata 根 —— 程序自身产生的东西（缓存 / 数据库 / 日志 / 设置 / 扩展）都挂在它下面，
/// 与 server 的 `SUWAYOMI_APPDATA_DIR` 同一语义。
///
/// 优先级：环境变量（显式外指）→ 安装包预置的 `appdataDir` → exe 同级的 `appdata/`。
/// 安装目录只读时要靠它把可写根外指，所以这个值必须**显式传给 server**，不能只靠 server
/// 自己推导。
fn appdata_dir() -> PathBuf {
    appdata_root(
        non_empty(std::env::var("SUWAYOMI_APPDATA_DIR").ok()),
        preset(),
        &base_dir(),
    )
}

/// 纯：appdata 根的解析。预置里的 `%VAR%` 在这里按进程环境展开。
fn appdata_root(env: Option<String>, preset: &PresetSettings, base: &Path) -> PathBuf {
    env.map(PathBuf::from)
        .or_else(|| preset.appdata_dir.as_deref().and_then(expand_env_path))
        .unwrap_or_else(|| base.join(APPDATA_DIR_NAME))
}

fn ensure_appdata_dirs(appdata: &Path) -> Result<(), TrayError> {
    APPDATA_SUBDIRS
        .iter()
        .try_for_each(|d| std::fs::create_dir_all(appdata.join(d)))?;
    Ok(())
}

/// 托盘设置文件：`<appdata>/settings/tray.json`，与 server 的设置（trackers.json、
/// 源偏好）同一个目录。
fn settings_path() -> PathBuf {
    appdata_dir().join(TRAY_SETTINGS_REL)
}

/// 安装包预置文件的落点：exe 同级的 `<appdata>/settings/tray.json` —— 与上面**同名同址**。
/// 只有 per-machine 安装（安装目录不可写）下那份才存在，那时用户设置写在
/// `%LOCALAPPDATA%\Suwayomi`，两者不会互相覆盖。
fn preset_settings_path() -> PathBuf {
    base_dir().join(APPDATA_DIR_NAME).join(TRAY_SETTINGS_REL)
}

/// 日志目录：`<appdata>/logs`（server / tray / sandbox 三个日志同处）。与 server
/// 子进程自己算出来的那个目录是同一个。
fn logs_dir() -> PathBuf {
    appdata_dir().join("logs")
}

/// 工作数据目录解析：设置里的自定义目录 → 预置的 `dataDir` → `<exe 同级>/data`
fn data_dir_of(s: &Settings) -> PathBuf {
    data_root(s.data_dir.as_deref(), preset(), &base_dir())
}

/// 纯：数据根的解析（同 `appdata_root`）。设置里手写的值也允许用 `%VAR%`。
fn data_root(override_dir: Option<&str>, preset: &PresetSettings, base: &Path) -> PathBuf {
    override_dir
        .and_then(expand_env_path)
        .or_else(|| preset.data_dir.as_deref().and_then(expand_env_path))
        .unwrap_or_else(|| base.join("data"))
}

/// 纯：`host` / `host:port` / `[v6]:port` → host 部分（源比对用）。
fn authority_host(authority: &str) -> &str {
    match authority.strip_prefix('[') {
        // IPv6 字面量：方括号里本身带冒号，不能按冒号切
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => authority.split(':').next().unwrap_or(authority),
    }
}

/// 纯：WebUI 的目标地址。`address` 留空 = 本机回环 + 实际端口；填了就用它，
/// 只给 host（不带端口）时补上实际端口 —— 用户往往只想换机器，不想换端口。
fn webui_url(address: Option<&str>, port: u16) -> String {
    let Some(addr) = address.map(str::trim).filter(|a| !a.is_empty()) else {
        return format!("http://127.0.0.1:{port}");
    };
    let (scheme, rest) = addr.split_once("://").unwrap_or(("http", addr));
    let authority = rest.split('/').next().unwrap_or(rest);
    // 只有 IPv6 字面量自带冒号，那种形式必须看到 `]:` 才算带了端口
    let has_port = if authority.starts_with('[') {
        authority.contains("]:")
    } else {
        authority.contains(':')
    };
    if has_port {
        format!("{scheme}://{authority}")
    } else {
        format!("{scheme}://{authority}:{port}")
    }
}

/// 本次运行实际使用的端口对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ports {
    /// server 的 HTTP 端口：设置里写死的那个，或自动档嗅探所得。
    server: u16,
    /// ext-runtime JVM 的端口。恒不等于 `server` —— 沙盒启动时会清掉占用者
    /// （`SandboxProcess::start`），撞上就是 server 被自己拉起的沙盒杀掉。
    sandbox: u16,
}

/// 从 `from` 起取第一个 `free` 的端口，最多试 `tries` 个（到 65535 为止）。
fn first_free_port(from: u16, tries: u16, free: impl Fn(u16) -> bool) -> Option<u16> {
    (0..tries)
        .map_while(|i| from.checked_add(i))
        .find(|p| free(*p))
}

/// 纯：端口对。`None` = 写死的端口用不了，或自动档下 [`PORT_SNIFF_TRIES`] 个候选
/// 全被占 —— 两种情况调用方都该报错，而不是硬塞一个：写死的端口被换掉后，用户按
/// 原端口配的防火墙/端口转发全部失效，且界面上看不出服务搬去了哪。
/// `free` 由调用方注入（真实实现是「能不能 bind」），便于断言。
fn resolve_ports(requested: Option<u16>, free: impl Fn(u16) -> bool) -> Option<Ports> {
    let server = match requested {
        Some(p) if free(p) => p,
        Some(_) => return None,
        None => first_free_port(DEFAULT_PORT, PORT_SNIFF_TRIES, &free)?,
    };
    // 挑不出来就让沙盒自己顺延，而不是拒绝启动：扩展不可用不该挡住书架/阅读
    let sandbox = first_free_port(SANDBOX_PORT_DEFAULT, PORT_SNIFF_TRIES, |p| {
        p != server && free(p)
    })
    .unwrap_or(SANDBOX_PORT_DEFAULT);
    Some(Ports { server, sandbox })
}

/// WebView 内只允许 WebUI 自身源的顶层导航：回环，或设置里指定的那个服务器地址。
/// 少了后者，指向远端 server 时 WebUI 自身的跳转会被误判成外部链接、丢给系统浏览器。
fn is_webui_origin(url: &tauri::Url, address: Option<&str>) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if matches!(host, "127.0.0.1" | "localhost") {
        return true;
    }
    address
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .is_some_and(|addr| {
            let (_, rest) = addr.split_once("://").unwrap_or(("http", addr));
            let authority = rest.split('/').next().unwrap_or(rest);
            host.eq_ignore_ascii_case(authority_host(authority))
        })
}

/// 纯：server 二进制的候选路径（按优先级）。不含托盘自身的 exe —— 否则 server
/// 缺失时会把托盘自己当 server 反复 spawn（fork 炸弹）。
fn server_bin_candidates() -> impl Iterator<Item = PathBuf> {
    // 惰性：调用方找到第一个存在的候选就短路，不为整条 PATH 分配 Vec
    let from_env = std::env::var("SUWAYOMI_BIN").ok().map(PathBuf::from);
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let from_path = std::env::var("PATH")
        .ok()
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>());

    from_env
        .into_iter()
        .chain(beside_exe.into_iter().flat_map(|dir| {
            let in_bin = SERVER_PROC_NAMES.map(|n| dir.join("bin").join(n));
            let in_dir = SERVER_PROC_NAMES.map(|n| dir.join(n));
            in_bin.into_iter().chain(in_dir)
        }))
        .chain(from_path.flat_map(|dir| SERVER_PROC_NAMES.map(move |n| dir.join(n))))
}

/// 纯：拉起 server 所需的环境变量（顺序无关，便于断言）。
///
/// 可写根必须显式传：server 自己的兜底是「exe 同级的 `appdata/`」，而安装目录只读时
/// 那个位置不可写，托盘才知道该指到哪。
fn server_env(data: &Path, ports: Ports, base: &Path, appdata: &Path) -> Vec<(String, OsString)> {
    vec![
        ("SUWAYOMI_PORT".into(), ports.server.to_string().into()),
        // 沙盒端口由托盘指定：两者默认值相邻，server 的监听端口自顺延时可能正好落到
        // 沙盒那一格，那样沙盒一启动就会把 server 杀掉（见 `SandboxProcess::start`）。
        (
            "SUWAYOMI_SANDBOX_PORT".into(),
            ports.sandbox.to_string().into(),
        ),
        (
            "SUWAYOMI_APPDATA_DIR".into(),
            appdata.as_os_str().to_os_string(),
        ),
        // 数据目录必须按解析结果显式传：server 自己的兜底链是 env → exe 发布布局 →
        // `cwd/data`，而 cwd 已经设成 data，最后那档会推成 `<data>/data`。
        // 下载 / 本地图源 / 自动备份都在这个根之下，没有各自的变量。
        ("SUWAYOMI_DATA_DIR".into(), data.as_os_str().to_os_string()),
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
    /// 固定 URL 解析失败（只可能是常量写错）
    Url(Box<dyn std::error::Error + Send + Sync>),
    InvalidPort(u16),
    /// 设置里写死的端口用不了（自动档嗅探会绕开，走不到这里）
    PortUnavailable(u16),
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
            Self::InvalidPort(p) => write!(f, "端口无效: {p}（应为 1-65535）"),
            Self::PortUnavailable(p) => write!(
                f,
                "端口 {p} 不可用（被占用或被系统保留）。请在设置里换一个端口，\
                 或清空端口让托盘自动选择"
            ),
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
            Self::Url(e) => Some(&**e),
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

/// 调试日志：写入 <appdata>/logs/tray.log（release GUI 无控制台，eprintln 不可见）。
/// 每条日志重新 open→写→关闭，故意不用 BufWriter：崩溃强杀时缓冲区内容会一起丢；
/// 目录只在首次 open 失败时才补建，避免每条日志都白跑一次 create_dir_all。
fn write_tray_log(msg: &str) {
    let dir = logs_dir();
    let path = dir.join("tray.log");
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
    };

    let mut file = match open() {
        Ok(f) => f,
        // 目录可能还不存在：补建后重试一次，仍失败就放弃这条日志
        Err(_) => match std::fs::create_dir_all(&dir).and_then(|_| open()) {
            Ok(f) => f,
            Err(_) => return,
        },
    };
    // INTENTIONAL: 日志写不进去不影响托盘功能，且没有更上层可传播
    let _ = writeln!(file, "{msg}");
}

/// 日志汇：把"写磁盘"这个副作用做成显式、可替换的依赖（默认写文件，测试可换内存 sink）。
/// `Clone`（`Arc` 包裹）使 actor 线程与调用方各持一份；经 `app.manage`/`app.state` 注入，
/// 或在无 `AppHandle` 处把 `&Logger` 显式传入。
#[derive(Clone)]
struct Logger {
    sink: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Logger {
    /// 默认 sink：写 <appdata>/logs/tray.log（release GUI 无控制台，eprintln 不可见）。
    fn file() -> Self {
        Logger {
            sink: Arc::new(write_tray_log),
        }
    }

    /// 写一条调试日志。失败（磁盘满/权限）不影响托盘功能，也没有更上层可传播。
    fn record(&self, msg: &str) {
        (self.sink)(msg);
    }

    /// 最佳努力操作：失败只留痕、不中断，替代 `let _ =` 静默吞错
    /// （`anti-empty-catch`）。用于"失败也没别的办法，但值得在日志里看见"的调用。
    fn best_effort<T, E: std::fmt::Display>(&self, what: &str, result: Result<T, E>) {
        if let Err(e) = result {
            self.record(&format!("[tray] {what}: {e}"));
        }
    }
}

/// 预置文件的读取结果：设置 + 值得留痕的原因。**这里不写日志** —— 日志目录由 appdata 根
/// 推出，而那个根正要用这份结果，在日志里绕一圈会回到这里。留痕由 `log_preset_warning`
/// 在启动时补一次。
fn preset_state() -> &'static (PresetSettings, Option<String>) {
    static STATE: OnceLock<(PresetSettings, Option<String>)> = OnceLock::new();
    STATE.get_or_init(|| {
        let path = preset_settings_path();
        match std::fs::read_to_string(&path) {
            // 没有预置文件是正常状态：绿色版、per-user 安装、MSIX
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (PresetSettings::default(), None),
            Err(e) => (PresetSettings::default(), Some(format!("读取失败（{e}）"))),
            Ok(text) => match serde_json::from_str::<PresetSettings>(&text) {
                Ok(p) => (p, None),
                Err(e) => (PresetSettings::default(), Some(format!("解析失败（{e}）"))),
            },
        }
    })
}

/// 进程内固定的那一份预置设置（文件只读一次）。
fn preset() -> &'static PresetSettings {
    &preset_state().0
}

/// 启动时留痕一次：预置文件在、但读不出来时，用户看到的是「预置没生效」。
fn log_preset_warning(log: &Logger) {
    if let Some(why) = &preset_state().1 {
        log.record(&format!(
            "[tray] WARN: {} {why}，忽略预置设置",
            preset_settings_path().display()
        ));
    }
}

/// 读设置。缺文件 / 解析失败都回落到默认值，但**留痕** —— 否则会表现成
/// "设置没生效"。
fn load_settings(log: &Logger) -> Settings {
    match std::fs::read_to_string(settings_path()) {
        Ok(text) => match serde_json::from_str::<Settings>(&text) {
            Ok(s) => s,
            Err(e) => {
                log.record(&format!(
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
fn find_server_bin(log: &Logger) -> Option<PathBuf> {
    let found = server_bin_candidates().find(|p| p.is_file());
    if found.is_none() {
        // 只在失败路径上再枚举一次候选表（惰性迭代器已被消费），换掉一次克隆
        let candidates: Vec<_> = server_bin_candidates().collect();
        log.record(&format!(
            "[tray] WARN: server binary not found (set SUWAYOMI_BIN or place \
             suwayomi-server next to this exe); candidates: {candidates:?}"
        ));
    }
    found
}

/// 当前所有 LISTENING 的端口（`netstat -ano` 一次拿全，省得每个候选起一次进程）。
fn listening_ports() -> HashSet<u16> {
    let Ok(out) = Command::new("netstat").args(["-ano"]).output() else {
        // INTENTIONAL: 拿不到就当作「谁都没监听」—— 后面还有试绑兜底
        return HashSet::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.to_ascii_lowercase().contains("listening"))
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter_map(|addr| addr.rsplit(':').next())
        .filter_map(|p| p.parse::<u16>().ok())
        .collect()
}

/// 端口是否可用。两件事都要查，各自都漏：
///
/// * **试绑**：Windows 的动态保留段（Hyper-V 把端口段留下自用，bind 直接报 10013）
///   里一个监听者都没有，只有真的去 bind 才发现它不可用。
/// * **netstat**：Windows 上 `0.0.0.0:P` 被占时 `127.0.0.1:P` 仍能绑上，而 server
///   绑的是 `0.0.0.0`（`SUWAYOMI_IP` 默认值），只看试绑会把这种占用当成空闲。
///
/// 试绑探回环而非通配：通配监听会让防火墙为托盘 exe 弹一次放行询问。
fn port_free(port: u16, listening: &HashSet<u16>) -> bool {
    !listening.contains(&port)
        && std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
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
            // INTENTIONAL: 强杀兜底，无错误细节可暴露；失败由下一轮启动自愈清理残留
            p.kill();
        }
    }
}

/// 请求 server 优雅关闭（POST loopback /api/v1/shutdown：停 postgres、杀 JVM 沙盒）
fn request_graceful_shutdown(port: u16, log: &Logger) {
    log.record(&format!(
        "[tray] requesting graceful shutdown on port {port}"
    ));
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return;
    };
    // 超时必须在 read 之前设置，否则 server 不响应时会永久阻塞
    // INTENTIONAL: 尽最大努力通知，未送达还有强杀兜底
    log.best_effort(
        "set read timeout",
        stream.set_read_timeout(Some(Duration::from_secs(2))),
    );
    let req = format!(
        "POST /api/v1/shutdown HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    log.best_effort("send shutdown request", stream.write_all(req.as_bytes()));
    let mut buf = [0u8; 256];
    log.best_effort("read shutdown response", stream.read(&mut buf));
}

/// 优雅停 server：请求 shutdown → 轮询等待 → 超时强杀兜底 → 回收子进程句柄。
/// 强杀残留的 postgres 子进程由 server 下次启动自愈清理。
fn stop_server(port: u16, child: &mut Option<Child>, log: &Logger) {
    if server_running(port) {
        request_graceful_shutdown(port, log);
        if poll_until(|| !server_running(port), POLL_EVERY, STOP_POLL_TIMES) {
            log.record("[tray] server exited gracefully");
        } else {
            log.record("[tray] graceful shutdown timed out; force-killing server");
            kill_server_processes();
        }
    }
    // 只回收句柄：Windows 父进程退出本就不杀子进程
    if let Some(mut c) = child.take() {
        // INTENTIONAL: 纯清理，wait 失败只意味着子进程已被别人回收
        log.best_effort("reap server child", c.wait());
    }
}

/// Poll TCP until the server accepts connections.
fn wait_ready(port: u16, timeout: Duration) -> bool {
    let millis = timeout.as_millis() / POLL_EVERY.as_millis().max(1);
    // u128 → usize 是窄化转换，显式 TryFrom 而不是 `as`（num-cast-try-from）
    let times = usize::try_from(millis).unwrap_or(usize::MAX);
    poll_until(|| port_open(port), POLL_EVERY, times)
}

/// 等 server 就绪并记录结果；超时只告警不阻断，是否致命由调用方决定。
fn await_ready(port: u16, timeout: Duration, log: &Logger) {
    if !wait_ready(port, timeout) {
        log.record(&format!(
            "[tray] WARN: server on port {port} not ready within {}s",
            timeout.as_secs()
        ));
    }
}

/// 数据子目录（autobackup/downloads/local）不存在时创建
fn ensure_data_dirs(data: &Path) -> Result<(), TrayError> {
    DATA_SUBDIRS
        .iter()
        .try_for_each(|d| std::fs::create_dir_all(data.join(d)))?;
    Ok(())
}

/// `inherit_stdio=true` 前台模式直接继承控制台；否则输出落 <appdata>/logs/server.log
fn spawn_server(
    data: &Path,
    ports: Ports,
    inherit_stdio: bool,
    log: &Logger,
) -> Result<Child, TrayError> {
    let bin = find_server_bin(log).ok_or(TrayError::ServerNotFound)?;
    let base = base_dir();
    let appdata = appdata_dir();
    ensure_appdata_dirs(&appdata)?;
    let logs = logs_dir();
    std::fs::create_dir_all(&logs)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("server.log"))?;

    let mut command = Command::new(&bin);
    command.current_dir(data);
    for (k, v) in server_env(data, ports, &base, &appdata) {
        command.env(k, v);
    }
    if !inherit_stdio {
        command
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file));
    }

    let child = command.spawn()?;
    if inherit_stdio {
        eprintln!(
            "[tray] spawned server {:?} on port {} (sandbox {})",
            bin, ports.server, ports.sandbox
        );
    }
    Ok(child)
}

// ─────────────────────────── 5. 监督者 actor ───────────────────────────

/// 监督者线程私有的状态：server 子进程的唯一所有者。
struct ActorState {
    settings: Settings,
    /// 本次运行实际使用的端口（`settings.server_port` 为 `None` 时是嗅探结果）
    ports: Ports,
    data: PathBuf,
    child: Option<Child>,
    /// false = 托盘退出后不再关停 server（「隐藏托盘」，或已自行发起关闭）
    attached: bool,
    /// 注入的日志汇：actor 线程里所有留痕都走它
    log: Logger,
}

/// 对外暴露的不可变快照。
#[derive(Debug, Clone)]
struct Runtime {
    settings: Settings,
    data: PathBuf,
    /// 实际监听中的端口。WebUI 窗口、关停请求、存活判定一律以它为准 ——
    /// 用设置里的「请求值」在自动档下会指向一个没人监听的端口。
    ports: Ports,
}

enum SupervisorMsg {
    /// 落盘新配置，并按新配置（重新）拉起 server
    Apply {
        settings: Settings,
        reply: Sender<Result<(), TrayError>>,
    },
    /// 仅落盘配置，不重启 server（「保存」按钮；生效需另行 Restart）
    Save {
        settings: Settings,
        reply: Sender<Result<(), TrayError>>,
    },
    /// 按当前配置重启
    Restart {
        reply: Sender<Result<(), TrayError>>,
    },
    /// 托盘退出后不再关停 server
    Detach { reply: Sender<()> },
    /// 取一份运行时快照
    Snapshot { reply: Sender<Runtime> },
    /// 收尾：按 attached 决定是否发关闭请求，然后线程退出
    Shutdown { reply: Sender<()> },
}

/// 监督者的句柄：只有消息通道、一个 join handle，以及一个注入的日志汇，
/// 状态本身拿不到。
#[derive(Clone)]
struct Supervisor {
    tx: Arc<Sender<SupervisorMsg>>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
    log: Logger,
}

impl Supervisor {
    fn spawn(state: ActorState) -> Supervisor {
        let log = state.log.clone();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || supervisor_loop(state, rx));
        Supervisor {
            tx: Arc::new(tx),
            thread: Arc::new(Mutex::new(Some(thread))),
            log,
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

    /// 仅落盘配置（不重启）；运行态仍以旧配置为准，重启时才切换。
    fn save(&self, settings: Settings) -> Result<(), TrayError> {
        self.ask(|reply| SupervisorMsg::Save { settings, reply })?
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
        // INTENTIONAL: 进程正在退出，断开或失败只能放弃
        self.log.best_effort(
            "stop supervisor",
            self.ask(|reply| SupervisorMsg::Shutdown { reply }),
        );
        let handle = self.thread.lock().ok().and_then(|mut guard| guard.take());
        if let Some(h) = handle {
            // INTENTIONAL: join 返回 Box<dyn Any> 无 Display；panic 时通道已断
            let _ = h.join();
        }
    }
}

fn supervisor_loop(state: ActorState, rx: Receiver<SupervisorMsg>) {
    let mut state = state;

    for msg in rx {
        // INTENTIONAL: 提问方可能已超时离开，send 失败不影响已提交的状态
        match msg {
            SupervisorMsg::Apply { settings, reply } => {
                let _ = reply.send(apply_settings(&mut state, settings));
            }
            SupervisorMsg::Save { settings, reply } => {
                let _ = reply.send(save_only(&mut state, settings));
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
                    ports: state.ports,
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
        request_graceful_shutdown(state.ports.server, &state.log);
    }
    drop(state.child);
}

/// 选定本次运行的端口。自动档下端口可能被换掉，这是唯一会留下痕迹的地方。
///
/// `ours` 用来豁免「这个端口现在是**我们自己**的 server 在监听」—— 重启时它会先被
/// 停掉，不豁免的话「重启服务端」在端口没改过时会被判成端口被占，永远重启不了。
fn pick_ports(
    settings: &Settings,
    ours: impl Fn(u16) -> bool,
    log: &Logger,
) -> Result<Ports, TrayError> {
    let listening = listening_ports();
    pick_ports_with(settings, |p| port_free(p, &listening), ours, log)
}

/// 同 [`pick_ports`]，只是把「端口空闲」的判据也做成入参，测试可以塞假探针。
fn pick_ports_with(
    settings: &Settings,
    is_free: impl Fn(u16) -> bool,
    ours: impl Fn(u16) -> bool,
    log: &Logger,
) -> Result<Ports, TrayError> {
    let free = |p: u16| is_free(p) || ours(p);
    let Some(ports) = resolve_ports(settings.server_port, free) else {
        let p = settings.server_port.unwrap_or(DEFAULT_PORT);
        log.record(&format!(
            "[tray] WARN: port {p} unavailable (occupied or reserved by the OS); \
             change it in settings, or leave it empty to let the tray pick one"
        ));
        return Err(TrayError::PortUnavailable(p));
    };
    // 端口被换掉时记一笔。写死档下换端口不会发生（`resolve_ports` 直接报错），所以
    // 这里描述的永远是自动档：起点默认端口用不了，向上顺延。
    let start = settings.server_port.unwrap_or(DEFAULT_PORT);
    if start != ports.server {
        log.record(&format!(
            "[tray] port {start} unavailable; using {} instead",
            ports.server
        ));
    }
    Ok(ports)
}

/// 落盘 → 建目录 → 停旧的 → 起新的。内存状态紧跟着磁盘提交，中途失败也不分叉。
fn apply_settings(state: &mut ActorState, next: Settings) -> Result<(), TrayError> {
    // 端口先定下来：定不下来就整个不动（旧 server 照常跑），设置窗口能原样报错
    let ports = pick_ports(
        &next,
        |p| p == state.ports.server && server_running(p),
        &state.log,
    )?;
    save_settings_file(&next)?;
    let next_data = data_dir_of(&next);
    ensure_data_dirs(&next_data)?;
    std::fs::create_dir_all(&next_data)?;

    stop_server(state.ports.server, &mut state.child, &state.log);

    let child = spawn_server(&next_data, ports, false, &state.log);
    // 配置已落盘，内存立即跟进 —— 状态与磁盘保持一致，即便启动失败
    state.settings = next;
    state.ports = ports;
    state.data = next_data;

    match child {
        Ok(c) => {
            state.child = Some(c);
            await_ready(ports.server, READY_TIMEOUT, &state.log);
            Ok(())
        }
        Err(e) => {
            state
                .log
                .record(&format!("[tray] restart after settings change failed: {e}"));
            Err(e)
        }
    }
}

/// 仅落盘配置：不碰 server、不改内存运行态。新配置要生效，需另行「重启服务端」。
fn save_only(_state: &mut ActorState, next: Settings) -> Result<(), TrayError> {
    save_settings_file(&next)
}

/// 按当前配置重启：先优雅停（内部已等进程退出），再重新拉起。
fn restart_server(state: &mut ActorState) -> Result<(), TrayError> {
    // 端口先定下来：定不下来就不停旧 server，避免「重启失败 = 服务直接没了」
    let ports = pick_ports(
        &state.settings,
        |p| p == state.ports.server && server_running(p),
        &state.log,
    )?;
    let data = state.data.clone();

    state.log.record("[tray] restart: stopping running server");
    stop_server(state.ports.server, &mut state.child, &state.log);
    // 进程已退出，但端口释放可能还有延迟
    std::thread::sleep(Duration::from_secs(2));

    match spawn_server(&data, ports, false, &state.log) {
        Ok(c) => {
            state.child = Some(c);
            state.ports = ports;
            await_ready(ports.server, RESTART_READY_TIMEOUT, &state.log);
            Ok(())
        }
        Err(e) => {
            state.log.record(&format!("[tray] restart failed: {e}"));
            Err(e)
        }
    }
}

// ─────────────────────────── 6. 窗口 ───────────────────────────

/// 设置页前端：编译期内联，经自定义协议提供（纯 cargo build 不嵌 frontendDist）
const SETTINGS_HTML: &str = include_str!("../frontend/index.html");

fn build_settings_window(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow, TrayError> {
    let url = tauri::Url::parse("settings://localhost/index.html")
        .map_err(|e| TrayError::Url(Box::new(e)))?;
    WebviewWindowBuilder::new(app, SETTINGS_WINDOW_LABEL, WebviewUrl::CustomProtocol(url))
        .title("Suwayomi 托盘设置")
        .inner_size(480.0, 380.0)
        .resizable(false)
        .theme(Some(tauri::Theme::Dark))
        // 以不可见状态创建——启动时完全不闪现，托盘「设置」菜单才 show()
        .visible(false)
        .build()
        .map_err(TrayError::Tauri)
}

fn show_settings_window(app: &tauri::AppHandle) {
    let log = app.state::<Logger>().inner().clone();
    match app.get_webview_window(SETTINGS_WINDOW_LABEL) {
        Some(w) => {
            log.best_effort("show window", w.show());
            log.best_effort("focus window", w.set_focus());
        }
        // 无 WebView 引擎（精简系统）：设置窗口不可用
        None => log.record("[tray] settings window not available (no system webview)"),
    }
}

/// 打开/聚焦 WebUI 窗口（label "webui"，已存在→show+focus，销毁后重建）。
/// 返回 false = 系统 WebView 不可用，调用方应回退系统浏览器。
fn open_webui_window(app: &tauri::AppHandle, rt: &Runtime) -> bool {
    let log = app.state::<Logger>().inner().clone();
    if let Some(w) = app.get_webview_window(WEBUI_WINDOW_LABEL) {
        log.best_effort("show window", w.show());
        log.best_effort("unminimize window", w.unminimize());
        log.best_effort("focus window", w.set_focus());
        log.record("[tray] webui window exists; focusing existing window");
        return true;
    }

    let target = webui_url(rt.settings.server_address.as_deref(), rt.ports.server);
    let Ok(url) = tauri::Url::parse(&target) else {
        log.record(&format!("[tray] invalid webui url: {target}"));
        return false;
    };

    match WebviewWindowBuilder::new(app, WEBUI_WINDOW_LABEL, WebviewUrl::External(url))
        .title("Suwayomi")
        .inner_size(1280.0, 860.0)
        // WebUI 里的外部链接（关于/文档、追踪器授权页等）一律交系统浏览器：拦截顶层导航
        // 与 target=_blank/window.open 新窗请求，不在 WebView 里打开
        .on_navigation({
            let log = log.clone();
            let address = rt.settings.server_address.clone();
            move |url| {
                if is_webui_origin(url, address.as_deref()) {
                    return true;
                }
                log.record(&format!("[tray] external link -> browser: {url}"));
                log.best_effort("open in browser", open::that(url.to_string()));
                false
            }
        })
        .on_new_window({
            let log = log.clone();
            move |url, _features| {
                log.record(&format!(
                    "[tray] external link (new window) -> browser: {url}"
                ));
                log.best_effort("open in browser", open::that(url.to_string()));
                tauri::webview::NewWindowResponse::Deny
            }
        })
        .build()
    {
        Ok(w) => {
            // 不 set_icon：标题栏与任务栏共用 WM_SETICON，set 后任务栏也会变
            log.best_effort("show window", w.show());
            log.best_effort("focus window", w.set_focus());
            log.record(&format!("[tray] opened webui window: {target}"));
            true
        }
        Err(e) => {
            log.record(&format!(
                "[tray] system webview unavailable ({e}); falling back to system browser"
            ));
            false
        }
    }
}

/// 打开 WebUI：设置开启且 WebView 窗口可用 → 窗口，否则系统浏览器
fn launch_webui(app: &tauri::AppHandle, rt: &Runtime) {
    let log = app.state::<Logger>().inner().clone();
    if rt.settings.prefer_web_view && open_webui_window(app, rt) {
        return;
    }
    let url = webui_url(rt.settings.server_address.as_deref(), rt.ports.server);
    log.record(&format!("[tray] opening webui in system browser: {url}"));
    log.best_effort("open in browser", open::that(url));
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
            Self::Start => "启动 Suwayomi 服务",
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
        "重启 Suwayomi 服务"
    } else {
        "启动 Suwayomi 服务"
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
            MenuItem::with_id(app, action.id(), text, true, None::<&str>).map_err(TrayError::Tauri)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let refs = items
        .iter()
        .map(|i| i as &dyn IsMenuItem<tauri::Wry>)
        .collect::<Vec<_>>();
    Menu::with_items(app, &refs).map_err(TrayError::Tauri)
}

/// 单一解释器：所有托盘动作只在这里被执行（穷尽匹配，加菜单项时编译器强制处理）。
fn run_action(action: TrayAction, app: &tauri::AppHandle, supervisor: &Supervisor) {
    let log = app.state::<Logger>().inner().clone();
    match action {
        TrayAction::Start => {
            if let Err(e) = supervisor.restart() {
                log.record(&format!("[tray] start/restart server failed: {e}"));
            }
        }
        TrayAction::OpenWebUi => {
            if let Some(rt) = supervisor.runtime() {
                launch_webui(app, &rt);
            }
        }
        TrayAction::OpenData => {
            if let Some(rt) = supervisor.runtime() {
                log.best_effort("open data dir", open::that(&rt.data));
            }
        }
        TrayAction::Settings => show_settings_window(app),
        TrayAction::HideTray => {
            // 托盘退出、server 保持后台运行
            log.record("[tray] hide_tray: keeping server running, exiting tray");
            log.best_effort("detach supervisor", supervisor.detach());
            app.exit(0);
        }
        TrayAction::Quit => {
            // 托盘立即退出：只发优雅关闭请求，server 后台自行收尾。
            // 已自行发起，标记 detach 以免退出收尾时重复请求。
            if let Some(rt) = supervisor.runtime() {
                request_graceful_shutdown(rt.ports.server, &log);
            }
            log.best_effort("detach supervisor", supervisor.detach());
            app.exit(0);
        }
    }
}

// ─────────────────────────── 8. Tauri 命令 ───────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    /// 设置里写的端口；`null` = 自动（启动前嗅探一个能绑的）
    server_port: Option<u16>,
    /// 设置里写的服务器地址；空串 = 本机（此时 `web_ui_url` 由端口推出）
    server_address: String,
    /// 当前实际监听的端口。自动档下它由嗅探决定，与 `server_port` 无关
    effective_port: u16,
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
            server_address: rt.settings.server_address.clone().unwrap_or_default(),
            effective_port: rt.ports.server,
            data_dir: rt.data.display().to_string(),
            data_dir_override: rt.settings.data_dir.clone().unwrap_or_default(),
            open_web_ui_on_startup: rt.settings.open_web_ui_on_startup,
            prefer_web_view: rt.settings.prefer_web_view,
            web_ui_url: webui_url(rt.settings.server_address.as_deref(), rt.ports.server),
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
    supervisor: State<Supervisor>,
    server_port: Option<u16>,
    server_address: Option<String>,
    data_dir: Option<String>,
    open_web_ui_on_startup: bool,
    prefer_web_view: bool,
) -> Result<(), TrayError> {
    // 校验：写死的端口非法就拒绝，而不是悄悄 clamp 出一个用户没填过的数字。
    // `None`（空 = 自动）合法，由托盘在启动前挑一个能绑的端口。
    if let Some(p) = server_port.filter(|p| !(1..=65535).contains(p)) {
        return Err(TrayError::InvalidPort(p));
    }
    let next: Settings = SettingsPatch {
        server_port,
        server_address,
        data_dir: Some(data_dir.unwrap_or_default()),
        open_web_ui_on_startup: Some(open_web_ui_on_startup),
        prefer_web_view: Some(prefer_web_view),
    }
    .into();

    // 仅落盘：server 继续用旧配置运行；端口/数据目录要等「重启服务端」才生效
    supervisor.save(next)
}

/// 「重启服务端」按钮：用已保存的配置停掉旧 server 并重新拉起（端口/数据目录随之生效）。
#[tauri::command]
fn restart_server_cmd(
    app: tauri::AppHandle,
    supervisor: State<Supervisor>,
) -> Result<String, TrayError> {
    let log = app.state::<Logger>().inner().clone();
    // 端口可能变化，先关掉指向旧端口的 WebUI 窗口
    if let Some(w) = app.get_webview_window(WEBUI_WINDOW_LABEL) {
        log.best_effort("close webui window", w.close());
    }
    let settings = load_settings(&log);
    supervisor.apply(settings)?;
    // 端口可能被自动档换掉，回读运行态而不是照设置里的值拼 URL
    let rt = supervisor.runtime().ok_or(TrayError::SupervisorDown)?;
    Ok(webui_url(
        rt.settings.server_address.as_deref(),
        rt.ports.server,
    ))
}

#[tauri::command]
fn open_data_dir(supervisor: State<Supervisor>) -> Result<(), TrayError> {
    let rt = supervisor.runtime().ok_or(TrayError::SupervisorDown)?;
    open::that(&rt.data).map_err(|e| TrayError::Io(std::io::Error::other(e)))
}

#[tauri::command]
fn webui_url_cmd(supervisor: State<Supervisor>) -> Result<String, TrayError> {
    let rt = supervisor.runtime().ok_or(TrayError::SupervisorDown)?;
    Ok(webui_url(
        rt.settings.server_address.as_deref(),
        rt.ports.server,
    ))
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
    // 无图形会话降级路径没有 AppHandle 可托管 Logger，直接用默认文件 sink 注入
    let log = Logger::file();
    log_preset_warning(&log);
    let settings = load_settings(&log);
    let Ok(ports) = pick_ports(&settings, |_| false, &log) else {
        eprintln!("configured port is unavailable; change it in tray settings");
        std::process::exit(1);
    };
    let data = data_dir_of(&settings);
    if let Err(e) = ensure_data_dirs(&data) {
        eprintln!("failed to prepare data dir: {e}");
        std::process::exit(1);
    }

    if server_running(ports.server) {
        eprintln!(
            "suwayomi-server already running on port {}; nothing to do",
            ports.server
        );
        std::process::exit(1);
    }

    let mut child = match spawn_server(&data, ports, true, &log) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}.\nPlace it in ./bin/ or set SUWAYOMI_BIN=/path/to/suwayomi-server");
            std::process::exit(1);
        }
    };

    await_ready(ports.server, READY_TIMEOUT, &log);
    eprintln!(
        "suwayomi-server ready on {} (Ctrl-C to stop)",
        webui_url(settings.server_address.as_deref(), ports.server)
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

    // Logger 作为托管依赖注入：后续用 `app.state::<Logger>()` 取出
    app.manage(Logger::file());
    let log = app.state::<Logger>().inner().clone();
    log_preset_warning(&log);

    let settings = load_settings(&log);
    let data = data_dir_of(&settings);
    ensure_data_dirs(&data)?;

    // 端口先定下来。挑不出来时不拦启动：托盘照常驻留，设置窗口是用户改端口的唯一入口。
    let picked = pick_ports(&settings, |_| false, &log).ok();
    let ports = picked.unwrap_or(Ports {
        server: settings.server_port.unwrap_or(DEFAULT_PORT),
        sandbox: SANDBOX_PORT_DEFAULT,
    });

    // 探测只做一次：菜单文案与是否拉起 server 必须基于同一个答案
    let already_running = picked.is_some() && server_running(ports.server);
    log.record(&format!("[tray] setup: server_running={already_running}"));

    let mut child = None;
    if picked.is_none() {
        log.record("[tray] server not started: configured port is unavailable");
    } else if already_running {
        log.record("[tray] server already running; not starting another");
    } else {
        match spawn_server(&data, ports, false, &log) {
            Ok(c) => {
                child = Some(c);
                await_ready(ports.server, READY_TIMEOUT, &log);
            }
            Err(e) => log.record(&format!(
                "[tray] WARN: server not started ({e}); set SUWAYOMI_BIN or place \
                 suwayomi-server next to this exe"
            )),
        }
    }
    let started = child.is_some();

    // 设置窗口：不可见创建（托盘菜单唤起）；WebView 不可用仅告警不中断
    if let Err(e) = build_settings_window(app.handle()) {
        log.record(&format!(
            "[tray] settings window unavailable (no system webview?): {e}"
        ));
    }

    let supervisor = Supervisor::spawn(ActorState {
        settings: settings.clone(),
        ports,
        data: data.clone(),
        child,
        attached: true,
        log: log.clone(),
    });
    app.manage(supervisor.clone());

    let menu = build_menu(app.handle(), already_running || started)?;

    // 托盘小图标用专用 tray.png（缩放清晰）；窗口/任务栏仍走 exe ICO。
    // 图标已在 build.rs 编译期解码，运行时直接包成 Image。
    let icon = tauri::image::Image::new_owned(
        tray_icon::TRAY_ICON_RGBA.to_vec(),
        tray_icon::TRAY_ICON_W,
        tray_icon::TRAY_ICON_H,
    );
    let tray = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("Suwayomi")
        .show_menu_on_left_click(false)
        .icon(icon);

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
            // 解析菜单 id；未知 id 记日志返回，不静默吞掉
            let log = app.state::<Logger>().inner().clone();
            let Ok(action) = TrayAction::from_str(event.id.as_ref()) else {
                log.record(&format!("[tray] unknown menu id: {}", event.id.as_ref()));
                return;
            };
            if let Some(supervisor) = app.try_state::<Supervisor>() {
                run_action(action, app, &supervisor);
            }
        })
        .build(app)?;

    // 启动即打开 WebUI（设置可关）：本托盘拉起或 server 已在运行都要开
    if settings.open_web_ui_on_startup && (started || already_running) {
        launch_webui(
            app.handle(),
            &Runtime {
                settings,
                data,
                ports,
            },
        );
    }
    Ok(())
}

/// settings 关闭 = 隐藏（驻托盘）；webui 关闭 = 真销毁（下次打开重建）
fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != SETTINGS_WINDOW_LABEL {
        return;
    }
    let log = window.app_handle().state::<Logger>().inner().clone();
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        log.best_effort("hide settings window", window.hide());
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
        // 还没进 GUI / 没有 app 可托管 Logger，直接用默认文件 sink
        Logger::file().record(&format!("[tray] fatal: {e}"));
        eprintln!("[tray] fatal: {e}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<(), TrayError> {
    let app = tauri::Builder::default()
        // 单实例：重复启动时第二实例直接退出，聚焦已有窗口
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let log = app.state::<Logger>().inner().clone();
            log.record("[tray] single-instance: second launch detected, focusing existing window");
            if let Some(w) = app.get_webview_window(WEBUI_WINDOW_LABEL) {
                log.best_effort("show window", w.show());
                log.best_effort("unminimize window", w.unminimize());
                log.best_effort("focus window", w.set_focus());
            } else if let Some(w) = app.get_webview_window(SETTINGS_WINDOW_LABEL) {
                log.best_effort("show window", w.show());
                log.best_effort("focus window", w.set_focus());
            }
        }))
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            restart_server_cmd,
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
    use std::ffi::OsStr;

    fn settings(json: &str) -> Settings {
        serde_json::from_str::<Settings>(json).expect("parse settings")
    }

    #[test]
    fn missing_fields_fold_to_defaults() {
        let s = settings("{}");
        // 没写端口 = 自动档，由托盘启动前嗅探（不是把默认端口钉进设置里）
        assert_eq!(s.server_port, None);
        assert_eq!(s.server_address, None);
        assert_eq!(s.data_dir, None);
        assert!(s.open_web_ui_on_startup);
        assert!(s.prefer_web_view);
    }

    #[test]
    fn partial_file_keeps_unspecified_defaults() {
        let s = settings(r#"{"serverPort": 9000}"#);
        assert_eq!(s.server_port, Some(9000));
        assert_eq!(s.data_dir, None);
        assert!(s.prefer_web_view);
    }

    /// 自动档要能原样往返：写出去的形状读不回来，托盘下次启动就静默回到内置默认值，
    /// 表现成「设置没生效」。
    #[test]
    fn auto_port_round_trips_through_the_settings_file() {
        let s = Settings {
            server_port: None,
            server_address: None,
            data_dir: None,
            open_web_ui_on_startup: true,
            prefer_web_view: false,
        };
        let json = serde_json::to_string(&s).expect("serialize settings");
        assert!(json.contains(r#""serverPort":null"#), "{json}");
        let back = settings(&json);
        assert_eq!(back.server_port, None);
        assert!(!back.prefer_web_view);
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

    /// 服务器地址同理：「未设置」只有一种表示（留空 = 本机回环）
    #[test]
    fn blank_server_address_is_none() {
        assert_eq!(settings(r#"{"serverAddress": ""}"#).server_address, None);
        assert_eq!(settings(r#"{"serverAddress": "  "}"#).server_address, None);
        assert_eq!(
            settings(r#"{"serverAddress": " 192.168.1.10 "}"#)
                .server_address
                .as_deref(),
            Some("192.168.1.10")
        );
    }

    #[test]
    fn out_of_range_port_falls_back_to_auto() {
        // 0 是合法 u16 但不是合法端口 → 当作没写（自动档）
        assert_eq!(settings(r#"{"serverPort": 0}"#).server_port, None);
        assert_eq!(
            settings(r#"{"serverPort": 65535}"#).server_port,
            Some(65535)
        );
        // 超出 u16 连反序列化都过不去 → 整个文件判为损坏，回落全套默认
        assert!(serde_json::from_str::<Settings>(r#"{"serverPort": 70000}"#).is_err());
    }

    #[test]
    fn default_settings_comes_from_the_empty_patch() {
        assert_eq!(Settings::default().server_port, None);
        assert_eq!(Settings::default().data_dir, None);
    }

    /// 自动档：从默认端口起向上取第一个能绑的
    #[test]
    fn auto_port_walks_up_from_the_default() {
        assert_eq!(
            resolve_ports(None, |_| true).map(|p| p.server),
            Some(DEFAULT_PORT)
        );
        assert_eq!(
            resolve_ports(None, |p| p != DEFAULT_PORT).map(|p| p.server),
            Some(DEFAULT_PORT + 1)
        );
        // 全被占：不 panic、不返回半个状态
        assert_eq!(resolve_ports(None, |_| false), None);
    }

    /// 写死的端口不可用时报错，不悄悄换一个 —— 换了以后用户按原端口配的
    /// 防火墙/端口转发全部失效，界面上也看不出服务搬去了哪
    #[test]
    fn explicit_port_is_never_silently_replaced() {
        assert_eq!(
            resolve_ports(Some(9999), |_| true).map(|p| p.server),
            Some(9999)
        );
        assert_eq!(resolve_ports(Some(9999), |p| p != 9999), None);
    }

    /// 沙盒端口必须避开 server 端口：撞上时沙盒启动会清掉占用者，也就是杀掉 server
    #[test]
    fn sandbox_port_never_collides_with_the_server_port() {
        // 用户把 server 钉在沙盒的默认端口上 → 沙盒让位
        let ports = resolve_ports(Some(SANDBOX_PORT_DEFAULT), |_| true).expect("ports");
        assert_eq!(ports.server, SANDBOX_PORT_DEFAULT);
        assert_eq!(ports.sandbox, SANDBOX_PORT_DEFAULT + 1);

        // 自动档同样不撞
        let auto = resolve_ports(None, |_| true).expect("ports");
        assert_ne!(auto.sandbox, auto.server);

        // 沙盒端口一个空位都没有：不拒绝启动（扩展不可用不该挡住书架/阅读），
        // 退回默认值，由沙盒侧自己再顺延
        let ports = resolve_ports(Some(1234), |p| p == 1234).expect("ports");
        assert_eq!(ports.sandbox, SANDBOX_PORT_DEFAULT);
    }

    /// 试绑探测要真的能看出端口被占 —— 用临时端口，避免和别的测试抢固定端口
    #[test]
    fn port_free_sees_an_occupied_port() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind ephemeral");
        let port = listener.local_addr().expect("local addr").port();
        assert!(!port_free(port, &HashSet::new()));
        drop(listener);
        assert!(port_free(port, &HashSet::new()));
    }

    /// 只在通配地址上被占的端口也必须算「不可用」：Windows 上 `0.0.0.0:P` 被占时
    /// 回环仍能绑上，而 server 绑的正是通配 —— 只试绑会把这种占用当成空闲，
    /// 于是 server 自己顺延到别的端口，托盘拼出的 URL 指向没人监听的地址。
    #[test]
    fn port_free_sees_a_wildcard_listener() {
        let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind wildcard");
        let port = listener.local_addr().expect("local addr").port();
        assert!(
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok(),
            "前提：通配被占时回环仍可绑（否则这个测试证明不了 netstat 那一路有用）"
        );
        assert!(!port_free(port, &listening_ports()));
    }

    /// 「重启服务端」时端口上还坐着我们自己的 server：它不算被占。不豁免的话，
    /// 端口没改过的重启会被永久拒绝，而重启正是让设置生效的唯一入口。
    #[test]
    fn own_server_holding_the_port_does_not_block_a_restart() {
        let log = Logger {
            sink: Arc::new(|_: &str| {}),
        };
        let settings = settings(r#"{"serverPort": 9000}"#);
        // 探针说「9000 被占」，而占它的正是我们自己 → 放行
        let ports = pick_ports_with(&settings, |_| false, |p| p == 9000, &log).expect("重启应放行");
        assert_eq!(ports.server, 9000);

        // 占着它的是别人：拒绝，且报错里带上端口号
        let err = pick_ports_with(&settings, |_| false, |_| false, &log).expect_err("应拒绝");
        assert!(matches!(err, TrayError::PortUnavailable(9000)), "{err}");
    }

    #[test]
    fn data_dir_defaults_under_base() {
        // 设置里写死的目录与预置无关，在哪台机器上都一样
        assert_eq!(
            data_dir_of(&settings(r#"{"dataDir": "/srv/manga"}"#)),
            PathBuf::from("/srv/manga")
        );
        // 没写时才轮到默认层
        assert_eq!(
            data_root(None, &PresetSettings::default(), &base_dir()),
            base_dir().join("data")
        );
    }

    /// `%VAR%` 展开：查不到的变量名一律当作"没设置" —— 照原样留下会让托盘真的建出一个
    /// 叫 `%FOO%` 的目录。
    #[test]
    fn expand_vars_resolves_known_names_and_rejects_the_rest() {
        let lookup =
            |n: &str| (n == "LOCALAPPDATA").then(|| OsString::from(r"C:\Users\me\AppData\Local"));
        assert_eq!(
            expand_vars(r"%LOCALAPPDATA%\Suwayomi", lookup).as_deref(),
            Some(OsStr::new(r"C:\Users\me\AppData\Local\Suwayomi"))
        );
        // 没有变量的、有多个变量的
        assert_eq!(
            expand_vars("/srv/manga", lookup).as_deref(),
            Some(OsStr::new("/srv/manga"))
        );
        assert_eq!(
            expand_vars("%A%-%B%", |n| Some(OsString::from(n))).as_deref(),
            Some(OsStr::new("A-B"))
        );
        // 未定义变量 / 空名字（`%%`） / 落单的 `%`
        assert!(expand_vars(r"%FOO%\Suwayomi", lookup).is_none());
        assert!(expand_vars("a%%b", lookup).is_none());
        assert!(expand_vars("100%", lookup).is_none());
    }

    /// 预置的两个根只是**默认层**：设置里显式写过的一律优先，用户改过之后以那份为准。
    #[test]
    fn preset_roots_are_defaults_behind_the_settings() {
        let preset = PresetSettings {
            appdata_dir: Some(r"C:\Users\me\AppData\Local\Suwayomi".into()),
            data_dir: Some(r"C:\Users\me\Pictures\Suwayomi".into()),
        };
        assert_eq!(
            appdata_root(None, &preset, Path::new("/base")),
            PathBuf::from(r"C:\Users\me\AppData\Local\Suwayomi")
        );
        // 环境变量是显式外指，仍压过预置
        assert_eq!(
            appdata_root(Some("/opt/suwayomi".into()), &preset, Path::new("/base")),
            PathBuf::from("/opt/suwayomi")
        );
        assert_eq!(
            data_root(None, &preset, Path::new("/base")),
            PathBuf::from(r"C:\Users\me\Pictures\Suwayomi")
        );
        assert_eq!(
            data_root(Some("/srv/manga"), &preset, Path::new("/base")),
            PathBuf::from("/srv/manga")
        );
    }

    /// 没有预置文件（绿色版 / per-user 安装 / MSIX）时，两个根与从前完全一样。
    #[test]
    fn missing_preset_keeps_the_built_in_roots() {
        let none = PresetSettings::default();
        assert_eq!(
            appdata_root(None, &none, Path::new("/base")),
            Path::new("/base").join(APPDATA_DIR_NAME)
        );
        assert_eq!(
            data_root(None, &none, Path::new("/base")),
            Path::new("/base").join("data")
        );
    }

    /// 预置文件与用户设置文件同名同址 —— 安装包的预置组件按这个相对路径投放，
    /// 两边对不上就是"预置装了但没人读"。
    #[test]
    fn preset_file_sits_where_the_settings_file_would_be() {
        assert_eq!(TRAY_SETTINGS_REL, "settings/tray.json");
        assert_eq!(
            preset_settings_path(),
            base_dir().join(APPDATA_DIR_NAME).join(TRAY_SETTINGS_REL)
        );
    }

    /// 托盘的设置文件与日志必须落在 appdata 根之下，且子路径与 server 侧一致
    /// —— 对不上就会变成两个目录各写一半。
    #[test]
    fn settings_and_logs_sit_under_the_appdata_root() {
        let root = appdata_dir();
        assert_eq!(settings_path(), root.join("settings").join("tray.json"));
        assert_eq!(logs_dir(), root.join("logs"));
        assert_eq!(
            APPDATA_SUBDIRS,
            [
                "cache",
                "logs",
                "db",
                "settings",
                "extensions/apk",
                "extensions/bin"
            ]
        );
    }

    #[test]
    fn webui_url_defaults_to_loopback_and_honours_the_configured_address() {
        // 留空 = 本机回环 + 实际端口，与没有这个设置时完全一样
        assert_eq!(
            webui_url(None, DEFAULT_PORT),
            format!("http://127.0.0.1:{DEFAULT_PORT}")
        );
        assert_eq!(
            webui_url(Some("  "), DEFAULT_PORT),
            format!("http://127.0.0.1:{DEFAULT_PORT}")
        );
        // 只给 host 就补上实际端口；自带端口或协议的原样用
        assert_eq!(
            webui_url(Some("192.168.1.10"), 4569),
            "http://192.168.1.10:4569"
        );
        assert_eq!(
            webui_url(Some("192.168.1.10:80"), 4569),
            "http://192.168.1.10:80"
        );
        assert_eq!(
            webui_url(Some("https://nas.local"), 4569),
            "https://nas.local:4569"
        );
        assert_eq!(
            webui_url(Some("http://[::1]:4569/"), 4569),
            "http://[::1]:4569"
        );
    }

    #[test]
    fn only_webui_origins_are_allowed_in_the_webview() {
        let localhost = tauri::Url::parse("http://127.0.0.1:4567/x").unwrap();
        let named = tauri::Url::parse("http://localhost:4567").unwrap();
        let external = tauri::Url::parse("https://example.com").unwrap();
        let remote = tauri::Url::parse("http://192.168.1.10:4569/x").unwrap();
        assert!(is_webui_origin(&localhost, None));
        assert!(is_webui_origin(&named, None));
        assert!(!is_webui_origin(&external, None));
        // 指定了服务器地址后，那个源自己的跳转不能再被当成外部链接
        assert!(is_webui_origin(&remote, Some("192.168.1.10")));
        assert!(is_webui_origin(&remote, Some("http://192.168.1.10:4569")));
        assert!(!is_webui_origin(&remote, Some("nas.local")));
        assert!(!is_webui_origin(&external, Some("192.168.1.10")));
    }

    #[test]
    fn server_env_covers_every_dir_the_server_needs() {
        let env = server_env(
            Path::new("/data"),
            Ports {
                server: 1234,
                sandbox: 5678,
            },
            Path::new("/base"),
            Path::new("/appdata"),
        );
        let keys = env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>();
        for k in [
            "SUWAYOMI_PORT",
            "SUWAYOMI_SANDBOX_PORT",
            "SUWAYOMI_APPDATA_DIR",
            "SUWAYOMI_DATA_DIR",
            "SUWAYOMI_WEBUI_DIR",
        ] {
            assert!(keys.contains(&k), "missing {k}");
        }
        // 目录级旋钮只剩 appdata / data 两个根：按子目录拆分的、以及按用户数据项
        // （下载 / 本地图源）拆分的变量都不应再传。
        for k in [
            "SUWAYOMI_EXTENSIONS_DIR",
            "SUWAYOMI_JAR_DIR",
            "SUWAYOMI_SETTINGS_DIR",
            "SUWAYOMI_CACHE_DIR",
            "SUWAYOMI_DB_DIR",
            "SUWAYOMI_LOGS_DIR",
            "SUWAYOMI_LOCAL_SOURCE_DIR",
        ] {
            assert!(!keys.contains(&k), "不应再传 {k}");
        }
        let value = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(value("SUWAYOMI_PORT"), "1234");
        // 沙盒端口必须一并传给 server：不传它就用内置的 4568，撞上 server 的端口时
        // 沙盒启动会先把占用者清掉
        assert_eq!(value("SUWAYOMI_SANDBOX_PORT"), "5678");
        assert_eq!(value("SUWAYOMI_APPDATA_DIR"), "/appdata");
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
            ports: Ports {
                server: 4569,
                sandbox: SANDBOX_PORT_DEFAULT,
            },
        };
        let view = SettingsView::of(&rt);
        assert_eq!(view.data_dir, "/srv");
        assert_eq!(view.data_dir_override, "/srv");
        assert!(!view.prefer_web_view);
        // 自动档（settings 里没写端口）下 URL 跟的是**实际监听**的那个，不是默认值
        assert_eq!(view.server_port, None);
        assert_eq!(view.effective_port, 4569);
        assert_eq!(view.web_ui_url, webui_url(None, 4569));
    }

    /// 设置里指定了服务器地址，读模型与打开的地址都得跟着走
    #[test]
    fn settings_view_exposes_the_configured_server_address() {
        let rt = Runtime {
            settings: settings(r#"{"serverAddress":"192.168.1.10"}"#),
            data: PathBuf::from("/srv"),
            ports: Ports {
                server: 4569,
                sandbox: SANDBOX_PORT_DEFAULT,
            },
        };
        let view = SettingsView::of(&rt);
        assert_eq!(view.server_address, "192.168.1.10");
        assert_eq!(view.web_ui_url, "http://192.168.1.10:4569");
    }
}
