# 代码风格评估：函数式 vs 面向对象

- 对象：`src/main.rs`（重构前 735 行单文件）、`build.rs`、`frontend/index.html`
- 评估日期：2026-09-26
- 工具：`cargo clippy --all-targets`（结果见附录）

---

> **实施状态（2026-09-26 更新）**
>
> 本文 P0–P2 已在 `refactor/fp-style` 分支落地（commit `baf6f28`），P3 的
> **A（actor 化 `AppState`）** 一并完成。
>
> 两项推迟项已在本分支后续补齐：
>
> - **H（PNG 挪到构建期解码）** —— 已做。`build.rs` 用 `png` 解码
>   `icons/tray.png`，把 RGBA 字节 + 宽高常量写进 `OUT_DIR/tray_icon.rs`，`main.rs`
>   用 `include!` 在编译期内嵌；运行时不再依赖 `png`（`png` 已移入
>   `[build-dependencies]`），并删掉了 `decode_tray_icon` 与 `TrayError` 的
>   `Decode` / `IconFormat` 变体。图标格式错误现在是**构建错误**而非运行时 panic。
> - **D（`Logger` 注入）** —— 已做。新增 `Logger` 类型（`Arc<dyn Fn>` 可替换 sink，
>   默认 sink 写 `cache/logs/tray.log`），经 `app.manage(Logger::file())` 注入，各处用
>   `app.state::<Logger>()` 取出；无 `AppHandle` 的纯函数 / actor 线程 / 无图形会话
>   降级路径，则把 `&Logger` 作为参数显式传入。`tray_log` 自由函数已删除，
>   `best_effort` 改为 `Logger` 的方法。"会写磁盘"这个副作用从隐式全局变成显式依赖。
>
> 依赖精简（报告 01）整体搁置，待本文全部收尾后启动。
>
> **rust-skills 复查（leonardomso/rust-skills，265 条规则）**
>
> 装好该 skill 后对照重查，又落了一轮改动（同一分支，未单独开 commit 前累积）：
>
> - **`anti-empty-catch` / `err-result-over-panic`**：新增 `Logger::best_effort`
>   替代约 20 处静默 `let _ =`（`tray_log` 自身、`supervisor` 回包、`JoinHandle::join`
>   三处保留 `let _ =` 并标注 `// INTENTIONAL`：要么无错误可暴露，要么无上层可传播）。
>   `sysinfo::Process::kill()` 返回 `bool` 而非 `Result`，直接调用并标注 `// INTENTIONAL`，
>   不强行塞进 `best_effort`。
> - **`err-expect-bugs-only` / `anti-unwrap-abuse`**：`TrayError` 去掉全部 stringly-typed
>   变体（`Icon(String)` / `Menu(String)` / `Url(String)`），改为类型化
>   `Url(Box<dyn Error+Send+Sync>)` / `Tauri(tauri::Error)`；`Decode` / `IconFormat`
>   随 PNG 移入构建期被一并删除。
> - **`own-slice-over-vec`**：`server_bin_candidates` 由分配 `Vec` 改为惰性
>   `impl Iterator<Item = PathBuf>`，调用方命中即短路，失败路径才 `collect` 一次用于留痕。
> - `cargo clippy --all-targets` 零警告，14 个单测全绿，默认 `cargo build` 通过。
>   （注：工作副本 CRLF 会在下次 Git 触碰时被规范化为 LF，属正常。）

---

## 1. 结论摘要

**整体判断：这不是一份 OOP 风格的代码，而是一份"命令式过程式"代码——写得相当克制，但缺少函数式的两个核心纪律：数据/行为的分离，以及"纯内核 + 副作用外壳"的分层。**

| 维度 | 现状 | 评价 |
| --- | --- | --- |
| 类型建模 | 有 `Settings` / `SettingsView` 分离，无继承、无 trait 对象、无装箱多态 | ✅ 好 |
| 可变共享状态 | `AppState` 全局 + 4 个同步原语 + 5 处 `state::<AppState>()` 拉取 | ❌ 最重的问题 |
| 纯函数比例 | 约 12 个函数里只有 4 个是真纯函数 | ⚠️ 偏低 |
| 错误处理 | `Result<String, String>` 字符串化错误 + 20+ 处 `let _ =` 吞错 | ❌ |
| 副作用可见性 | `tray_log` / `expect` / `panic!` 藏在"看起来纯"的辅助函数里 | ❌ |
| 组合性 | 轮询、进程扫描等逻辑手写 `for`/`while`，无高阶函数复用 | ⚠️ |
| RAII / 析构副作用 | `impl Drop for AppState` 里发 HTTP 请求、管进程 | ❌ OOP 味最重的一处 |

clippy 默认规则干净（仅 1 条提示），说明"机械质量"没问题；本文关注的是 clippy 查不出的**架构性**风格问题。

---

## 2. 主要问题

### A. `AppState`：全局可变对象 + 析构函数副作用（OOP 味最重）

`main.rs:46-69`

```rust
struct AppState {
    port: AtomicU16,
    data_dir: Mutex<PathBuf>,
    server: Mutex<Option<Child>>,
    keep_server_on_exit: AtomicBool,
}

impl Drop for AppState {           // ← 析构函数里做 I/O 和进程信号
    fn drop(&mut self) {
        if !self.keep_server_on_exit.load(...) {
            request_graceful_shutdown(self.port.load(...));   // 发 HTTP
        }
        ...
    }
}
```

然后 `app.manage(AppState { .. })`（`:711`）注册成全局单例，5 个地方用 `app.state::<AppState>()` 反查（`:649, 656, 674, 679, 695, 701`）。

这是教科书式的 **service locator + mutable global object**。四个问题：

1. **复合更新非原子。** `save_settings`（`:397-437`）的执行顺序是：
   `stop_server_gracefully(old_port)` → `take/wait child` → `spawn_server` → `port.store` → `data_dir = new`。
   如果 `spawn_server` 在 `:429` 返回 `None` 提前 `?` 返回，**server 已经被杀掉，但 `state.port` 仍指向死端口、`state.server` 是 `None`**。进程实际状态和内存状态就此分叉。

2. **`Drop` 做的清理其实不保证执行。** `run_server_foreground`（`:464-496`）里有 4 处 `std::process::exit`，它**绕过所有析构函数**。"RAII 保证清理"在这个程序里是错觉。

3. **锁中毒即崩溃。** `state.data_dir.lock().unwrap()`（`:383, 435, 668, 680`）、`state.server.lock().unwrap()`（`:65, 425`）——任何一个命令 panic 之后，GUI 后续所有操作连锁 panic。

4. **无法测试。** 进程生命周期逻辑和 `tauri::AppHandle` 绑死，不启 GUI 就测不了。

**函数式重构：用消息传递替代共享可变状态（actor）。**

```rust
enum SupervisorMsg {
    Restart { data: PathBuf, port: u16 },
    Shutdown,
    Query(oneshot::Sender<SupervisorState>),
}

// 唯一的 Child 所有者；外部只能通过消息交互
fn supervisor_loop(rx: Receiver<SupervisorMsg>) { /* owns Child */ }
```

Tauri 命令退化成：`解析入参 → 发消息 → 等回复`，全部变成可测的纯逻辑。这能一次性消灭 4 个同步原语里的 3 个、消灭锁中毒、让复合更新天然原子（因为只有一个线程能改）。

---

### B. `impl Default for Settings`：手写构造 + 同一状态两种表示

`main.rs:35-44`

```rust
impl Default for Settings {
    fn default() -> Self { Self { server_port: 8090, data_dir: None, .. } }
}
```

手写 `Default` 把字段列表抄了一遍，魔数 `8090` 散落在默认值里；而 `data_dir: Option<String>` 同时用 `None`（`:82`）和 `""`（`:82` 的 `!d.trim().is_empty()`）表示"未设置"——**同一个语义两种编码**。

```rust
const DEFAULT_PORT: u16 = 8090;

#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    #[serde(default = "default_port")]
    server_port: u16,
    #[serde(default, deserialize_with = "blank_to_none")]  // "" → None，归一化到一种表示
    data_dir: Option<String>,
    ...
}
```

再进一步（真正的函数式做法）：反序列化到 `SettingsPatch`（全 `Option` 字段），再 `fold` 合并到默认值上——这样"缺字段"和"字段为空"的处理只有一处。

---

### C. 纯逻辑与副作用纠缠（6 处）

函数式最实用的一条纪律是 **functional core, imperative shell**：把计算（纯、可测）和 I/O（薄、无逻辑）分开。目前 6 个函数把两者搅在一起：

#### C1. `find_server_bin`（`:121-160`）

候选路径的生成（纯）和 `is_file()` 检查、5 次 `tray_log`（副作用）交错在一个函数里。

```rust
fn server_bin_candidates() -> impl Iterator<Item = PathBuf> {   // 纯：可穷举测试
    let exe = std::env::current_exe().ok();
    let env_override = std::env::var("SUWAYOMI_BIN").ok().map(PathBuf::from);
    env_override.into_iter()
        .chain(exe.as_deref().and_then(Path::parent).into_iter().flat_map(|dir| [
            dir.join("bin/suwayomi-server.exe"), dir.join("bin/suwayomi-server"),
            dir.join("suwayomi-server.exe"),    dir.join("suwayomi-server"),
        ]))
        .chain(path_dirs().flat_map(|d| [d.join("suwayomi-server.exe"), d.join("suwayomi-server")]))
}

fn find_server_bin() -> Option<PathBuf> {          // 副作用：只剩一行
    let found = server_bin_candidates().find(|p| p.is_file());
    if found.is_none() { tray_log("[tray] WARN: server binary not found"); }
    found
}
```

顺带把"绝不把托盘自己当 server"（`:132-133` 注释里提到的 fork 炸弹风险）变成一个可以对 `server_bin_candidates()` 做的**断言测试**。

#### C2. `spawn_server`（`:251-281`）

环境变量的计算是纯的，日志文件的创建和 `spawn` 是副作用。拆出纯的一半：

```rust
fn server_env(data: &Path, port: u16, logs: &Path) -> Vec<(String, PathBuf)> { .. }  // 纯，可断言
```

另外签名 `data: &PathBuf` 应改为 `&Path`（`clippy::ptr_arg` 类问题）。

#### C3/C4. `server_running`（`:165-176`）与 `kill_server_processes`（`:179-196`）

两个函数各自把"进程名匹配"这段谓词写了一遍。抽成一个；配合依赖报告的 §3.2，它们会后进一步简化为 `port_open(port)` 纯函数。

#### C5/C6. `stop_server_gracefully`（`:215-229`）与 `wait_ready`（`:232-241`）

两段几乎一样的手写轮询循环，各带 `sleep` 和循环内 `return`。抽成一个高阶函数：

```rust
/// 纯组合子：以固定间隔重试谓词，最多 n 次
fn poll_until(pred: impl Fn() -> bool, every: Duration, times: usize) -> bool {
    (0..times).any(|_| { std::thread::sleep(every); pred() })
}

fn wait_ready(port: u16, timeout: Duration) -> bool {
    poll_until(|| port_open(port), Duration::from_millis(500), (timeout.as_millis() / 500) as usize)
}
fn stop_server_gracefully(port: u16) {
    request_graceful_shutdown(port);
    if !poll_until(|| !port_open(port), Duration::from_millis(500), 12) {
        tray_log("[tray] graceful shutdown timed out; force-killing server");
        force_kill_server();
    }
}
```

两个循环塌缩成一个可测的组合子（给它传 `|| true` / `|| false` 即可单测）。

---

### D. 隐藏副作用：`tray_log` 与 20+ 处 `let _ =`

`tray_log`（`:284-294`）是个自由函数，直接从 `find_server_bin`、`decode_png_icon` 的调用链中写文件，且用 `let _ =` 吞掉 `io::Result`。调用方从签名上完全看不出"这个函数会写磁盘"。

同样被 `let _ =` 静默吞掉的还有：`:208, 209, 246, 254, 273, 342-344, 423, 427, 681`。

函数式的处理不是"禁止副作用"，而是**让副作用出现在类型里**：

- 短中期：把 `tray_log` 换成一个注入的 `Logger` 值（哪怕只是 `Arc<dyn Fn(&str)>`），让调用方签名显式声明它需要日志能力；
- 对吞错的地方：要么返回 `Result` 往上传播，要么显式收集成 `Vec<Warning>` 一起返回——不要 `let _ =`。

> **实施（refactor/fp-style）：已落地。** 自由函数 `tray_log` 删除，改为可注入的
> `Logger`（`Arc<dyn Fn>` sink，默认写 `cache/logs/tray.log`），经 `app.manage(Logger::file())`
> 注入、`app.state::<Logger>()` 取出；无 `AppHandle` 的纯函数 / actor 线程 / 无图形会话
> 降级路径，把 `&Logger` 作为参数显式传入；`best_effort` 成为 `Logger` 的方法。
> 约 20 处 `let _ =` 已被 `best_effort` 替换，仅剩三处保留并标注 `// INTENTIONAL`。

---

### E. 字符串化错误 + 校验与副作用混在一个函数里

`save_settings`（`:396-437`）返回 `Result<String, String>`，错误信息现场 `format!`（`:413, 418, 430`）：

```rust
.map_err(|e| format!("写入设置失败: {e}"))?
.ok_or_else(|| "server 启动失败（找不到 suwayomi-server 可执行文件）".to_string())?
```

问题：前端拿到的是中文句子，无法按错误类型分支；新加错误只能再拼一个字符串；`?` 无法跨类型自动转换。

```rust
#[derive(Debug)]
enum TrayError { Io(std::io::Error), Spawn(std::io::Error), ServerNotFound, InvalidPort(u16) }
impl From<std::io::Error> for TrayError { .. }
impl std::fmt::Display for TrayError { .. }
```

同时把这一个 40 行函数拆成两半：

```rust
fn normalize(input: SettingsInput) -> Result<Settings, TrayError>;   // 纯：校验 + 归一化
fn apply(app: &AppHandle, s: &Settings) -> Result<(), TrayError>;    // 副作用：落盘 + 重启
```

另外 `:405` 的 `server_port.clamp(1, 65535)` 会**静默改写用户输入**（填 0 变成 1）。函数式偏好"拒绝非法输入"而不是"悄悄修正"，或者至少把修正后的值返回给前端显示。

---

### F. 菜单分发：`match` 字符串 + 每个分支各自取全局状态

`main.rs:654-707`

```rust
.on_menu_event(|app, event| match event.id.as_ref() {
    "start_suwayomi" => { let st = app.state::<AppState>(); ... }   // 6 个分支各自重复取状态
    "open_webui"     => { let st = app.state::<AppState>(); ... }
    ...
    _ => {}      // ← 拼错 id 会静默失效
})
```

id 字符串在创建处（`:615-620`）和分发处（`:655-699`）各写一遍，靠人工对齐；`_ => {}` 兜底让任何拼写错误都不报错。

**函数式做法：parse, don't validate + 单一解释器。**

```rust
#[derive(Clone, Copy)]
enum TrayAction { Start, OpenWebUi, OpenData, Settings, HideTray, Quit }

impl TrayAction {
    const ALL: [TrayAction; 6] = [..];
    fn id(self) -> &'static str { .. }          // 唯一真源，创建与分发共用
}
impl FromStr for TrayAction { type Err = (); .. }   // 纯、可穷举测试

fn run(action: TrayAction, app: &AppHandle) {        // 唯一的解释器
    match action { .. }                             // 穷尽匹配，加菜单项时编译器强制处理
}
```

收益：id 只有一处定义；`match` 变成穷尽的（新增动作时编译器报错，而不是运行时静默）；动作集合变成可单测的数据。

---

### G. `main()` 是 200 行的 god function

`main.rs:532-735`：无头检测、插件、handler 注册、协议、setup（读设置 → 起进程 → 建窗口 → 建菜单 → 建托盘 → 开 WebUI）、窗口事件，全在一个 builder 链 + 一个巨型 `.setup(|app| { .. })` 闭包里。

拆成具名阶段：

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    maybe_run_headless();
    tauri::Builder::default()
        .plugin(single_instance())
        .invoke_handler(tauri::generate_handler![..])
        .register_uri_scheme_protocol("settings", settings_page)   // 抽成具名 fn
        .setup(setup)
        .on_window_event(on_window_event)
        .run(tauri::generate_context!())?;
    Ok(())
}

fn setup(app: &mut tauri::App) -> Result<(), TrayError> {
    let boot = bootstrap(app)?;                  // 纯：算端口/数据目录/是否已有 server
    build_settings_window(app)?;                 // (&App) -> Result<WebviewWindow>
    build_tray(app, &boot)?;                     // (&App, &Boot) -> Result<TrayIcon>
    if boot.should_open_webui { launch_webui(app.handle(), boot.port); }
    Ok(())
}
```

`build_menu` / `build_tray` / `build_settings_window` 都是 `(&AppHandle) -> Result<T>` 的具名函数，各自可独立阅读与测试。

---

### H. "纯"辅助函数里藏着 panic

| 位置 | 现状 | 问题 |
| --- | --- | --- |
| `:511, 513, 527` | `decode_png_icon` 里 `.expect("tray icon: png info")` / `panic!("unsupported png format")` | 输入是 `include_bytes!` 的**编译期常量**，资源写错只会在用户机器上崩。应在 `build.rs` 里解码，让错误变成**编译失败**（见依赖报告 §3.4） |
| `:320, 594` | `tauri::Url::parse(..).expect(..)` | 同样是常量，应 `OnceLock` 惰性初始化或在构建期校验 |
| `:116` | `to_string_pretty(settings).expect("serialize settings")` | 对该类型不可能失败；用类型不变式消除，而不是 `expect` |

> **实施（refactor/fp-style）：已落地。** `decode_png_icon`（重构后名 `decode_tray_icon`）
> 已删除，PNG 解码挪到 `build.rs`（`png` 移入 `[build-dependencies]`），图标格式错误
> 现在是**构建错误**而非运行时 panic。`:320` 的 `tauri::Url::parse` 早先已改为 `?`
> 传播并类型化为 `TrayError::Url`；`:116` 的序列化 `expect` 在重构中已改为 `?`。

---

### I. 两个实际缺陷（风格审查时发现的）

1. **`:209` 读超时设置在读之后——无效且可能永久阻塞。**

   ```rust
   let mut buf = [0u8; 256];
   let _ = stream.read(&mut buf);                                  // ← 阻塞读
   let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));  // ← 读完之后才设，毫无作用
   ```
   
   `set_read_timeout` 必须在 `read` **之前**调用。当前若 server 不响应，`request_graceful_shutdown` 会卡住调用线程（托盘 UI 线程）。

2. **`:574` 与 `:628` 两次独立调用 `server_running(port)`，结果可能不一致。**

   ```rust
   let running = server_running(port);     // :574  决定是否 spawn
   ...
   let running = server_running(port);     // :628  决定菜单文字
   ```
   
   两次之间 server 可能已启动/退出，导致菜单显示"启动 Suwayomi"但实际已在运行。纯函数化的自然结果就是把这类查询**计算一次、向下传递**。

   相关：`:576` 的 `let (server, _ready) = ...` 里 `_ready` 从未使用——`wait_ready` 的返回值在多处被丢弃（`:432, 585, 671` 的 `let _ =`），等于"启动失败静默"。

---

### J. 值得保留的函数式部分（不要改）

- `webui_url`（`:296`）、`is_webui_origin`（`:301-304`）、`data_dir_of`（`:80-85`）——真正的纯全函数，无隐藏状态。
- `include_str!` / `include_bytes!`（`:451, 634`）把资源变成编译期数据而不是运行时 I/O。
- `WebviewWindowBuilder` 的 builder 链 + `.on_navigation(...)` / `.on_new_window(...)` 高阶闭包（`:317-339`）——无状态的组合式构造，写得很对。
- `SettingsView`（`:367-378`）作为独立读模型，而不是把 `AppState` 直接抛给前端——这个分离是对的。
- `decode_png_icon` 的骨架（`&[u8] -> Image`）方向正确，只是不该 panic（见 H）。

---

## 3. 重构优先级

| 优先级 | 项 | 工作量 | 收益 |
| --- | --- | --- | --- |
| P0 | I-1 修 `set_read_timeout` 顺序；I-2 `server_running` 只算一次 | 15 min | 修掉一个真实卡死风险 |
| P0 | 依赖报告 §3.1 + §4.1/§4.2（feature、frontendDist、capability） | 30 min | −6 crate，拆掉定时炸弹 |
| P1 | C5/C6 抽 `poll_until`；C1 拆 `server_bin_candidates` | 1 h | 少 40 行，逻辑可测 |
| P1 | E 引入 `TrayError` 枚举，拆 `normalize` / `apply` | 2 h | 错误处理从字符串升级为类型 |
| P1 | H panic 搬进 build.rs | 1 h | 资源错误变成编译错误 | ✅ 已完成 |
| P2 | F `TrayAction` 枚举 + 单一解释器 | 2 h | 消除 id 拼写失效、穷尽匹配 |
| P2 | G 拆 `main()` 为具名阶段 | 2 h | 可读性 |
| P3 | A actor 化 `AppState` | 半天 | 消灭共享可变状态与锁中毒 |
| P3 | D `Logger` 注入 / 消灭 `let _ =` | 半天 | 副作用可见 | ✅ 已完成 |

建议 P0–P1 与依赖精简（报告 01）的对应步骤**合并成同一次改动**：它们动的是同一批函数（`find_server_bin`、`spawn_server`、`stop_server_gracefully`、`wait_ready`），分开改会重复返工。

---

## 附录：`cargo clippy --all-targets` 结果

```
warning: using `chunks_exact` with a constant chunk size
   --> src\main.rs:521:27
    |
521 |             for px in rgb.chunks_exact(3) {
    |                           ^^^^^^^^^^^^^^^ help: consider using `as_chunks` instead: `as_chunks::<3>().0`
    = note: `#[warn(clippy::chunks_exact_to_as_chunks)]` on default

warning: `suwayomi-tray` (bin "suwayomi") generated 1 warning
```

默认 lint 全部通过。补充建议（clippy 未覆盖）：

- `spawn_server(data: &PathBuf, ..)` → `&Path`
- `SERVER_PROC_NAMES.iter().any(|x| n == *x)` → `SERVER_PROC_NAMES.contains(&*n)`
- `ensure_data_dirs`（`:244-248`）的 `for` + `let _ =` → `["autobackup", "downloads", "local"].iter().try_for_each(...)` 并传播 `Result`
- `Settings` 派生了 `Clone` 但从未被 clone —— 删掉这个死 trait impl
- `on_window_event` 中 `window.label() == "settings"` 的字符串字面量，与 F 项一起收敛为常量
