# 依赖精简评估报告

- 项目：`suwayomi-tray` v0.1.0（Tauri 2 桌面壳）
- 工具链：cargo 1.98.1 / tauri 2.11.5 / tauri-build 2.6.3
- 评估日期：2026-09-26
- 复核脚本：`docs/migration/dep-cost.py`（重跑可复现本文的"独占包"数字）

---

## 1. 结论摘要

先说结论：**依赖本身不多（7 个运行时 + 1 个构建），真正拖大依赖树的是 Tauri 自己的默认 feature 和两个"可以自己写"的第三方库。**

| 指标 | 当前 |
| --- | --- |
| `Cargo.toml` 直接依赖 | 8（含 build-dependencies） |
| `Cargo.lock` 条目数 | **494** |
| lock 内不可达（僵尸）包 | **0**（lock 很干净，没有残留条目） |
| 实际参与编译的 crate 单元 · Windows | **329** |
| 实际参与编译的 crate 单元 · Linux | **453** |
| 实际参与编译的 crate 单元 · macOS | **327** |

按本文方案执行 **方案 1 + 2 + 3**（改动都在 `Cargo.toml` + 约 100 行 `main.rs`）：

| 平台 | 现在 | 优化后 | 减少 |
| --- | --- | --- | --- |
| Windows x64 | 329 | **299** | −30（−9%） |
| Linux x64 | 453 | **379** | −74（−16%） |
| macOS | 327 | **307** | −20（−6%） |

---

## 2. 逐个依赖的"真实成本"

494 这个数字没有意义——它包含了所有平台、所有 feature 的并集。有意义的是**"某平台实际编译多少个 crate"**，以及**"删掉 X 能省多少个"**。

用 `cargo tree --prefix none --target <triple>`（只做解析，不编译）逐项实测：

| 变更 | Windows | Linux | macOS | 说明 |
| --- | --- | --- | --- | --- |
| 基线 | 329 | 453 | 327 | |
| 去掉 `tauri/compression` | **−6** | **−6** | **−6** | brotli 全家桶 |
| 去掉 `sysinfo` | **−14** | **−6** | **−6** | rayon/ntapi/windows 0.57 |
| 去掉 `tauri-plugin-single-instance` | **−10** | **−62** | **−8** | zbus + async-* 全栈（Linux） |
| 去掉 `png` | −0 | −3 | −3 | Windows 上**完全免费** |
| 去掉 `open` | −1 | −3 | −1 | |
| 去掉 `serde_json` | −0 | −0 | −0 | tauri 已依赖，0 成本 |
| 去掉 `tauri/x11` + `tauri/dbus` | −0 | −9 | −0 | **不建议**，见 §3.6 |

### 2.1 关键发现：`png` 现在是"免费"的，但不是"必需"的

```
png@0.17.16  ← 被 suwayomi-tray、ico、tauri-codegen 同时使用
png@0.18.1   ← 被 muda、tray-icon 使用
```

`tauri-codegen`（经 `ico` 0.5）**已经**把 `png 0.17` 拉进来了，所以我们在 `Cargo.toml` 里再写一行 `png = "0.17"` **在 Windows 上不增加任何一个 crate**（实测 −0）。它只是消除了"版本万一漂移"的不确定性。

反过来说明一件事：**不要为了省依赖把 `png` 换成 `tauri::include_image!` / `Image::from_bytes`。** 那条路要开 `tauri/image-png` feature，而它等于 `image/png`，会把整个 `image 0.25` 拉进来——净增而不是净减。这是最容易踩的反向优化。

真正该做的是：把解码挪到**构建期**（见 §3.4）。

### 2.2 关键发现：`sysinfo` 是本仓库最不划算的依赖

`sysinfo` 只被用到两处（`main.rs:165` `server_running`、`:179` `kill_server_processes`），做的事是"按进程名扫描 + kill"。为此付出：

- Windows：`ntapi`、`windows 0.57`、`windows-core 0.57`、`windows-implement`、`windows-interface`、`windows-result 0.1.2`（**14 个 crate**）
- Linux：`rayon`、`rayon-core`、`crossbeam-deque`、`crossbeam-epoch`、`either`（**6 个 crate**）

而它的两个能力都可以用标准库 + 已有的 `std::process::Command` 等价替代（§3.2）。

### 2.3 关键发现：`tauri-plugin-single-instance` 的成本高度平台不对称

- **Linux：−62**（zbus 5.19 / zvariant / async-io / async-executor / blocking / polling / rustix / tempfile …）
- **Windows：−10**（只多一套 `windows-sys 0.60`）
- **macOS：−8**

也就是说：如果你只在 Windows 上发版，删掉它只省 10 个 crate，收益/风险比很差；**如果 CI 要跑 Linux 六个 target，它就是最大的单点成本**。

---

## 3. 建议方案（按性价比排序）

### 3.1 ✅ 强烈建议：关掉 `tauri` 的 `compression` feature（−6 / −6 / −6，风险低）

`tauri` 默认 feature 是 `["wry", "compression", "common-controls-v6", "dynamic-acl", "x11", "dbus"]`。其中 `compression` 会拉进 `brotli` + `brotli-decompressor` + `alloc-no-stdlib` + `alloc-stdlib`。

它压缩的是**内嵌的 `frontendDist` 静态资源**。但本项目的实际导航目标是：

- `WebviewUrl::CustomProtocol(settings://localhost/index.html)`（`main.rs:593`）
- `WebviewUrl::External(http://127.0.0.1:{port})`（`main.rs:320`）

**`tauri.conf.json` 里的 `"frontendDist": "frontend"` 其实从来没被访问过**（`app.windows: []`，两个窗口都是代码里建的）。所以：

```toml
# Cargo.toml
tauri = { version = "2", default-features = false,
          features = ["wry", "tray-icon", "common-controls-v6", "dynamic-acl", "x11", "dbus"] }
```

同时可以把 `tauri.conf.json` 的 `build.frontendDist` 一并删掉（需 `cargo build` 验证一次，理论上 `Option` 字段可缺省）。

> 注意：必须保留 `wry`（否则没有 WebView 运行时）、`tray-icon`、`common-controls-v6`（Windows 菜单视觉样式）。`dynamic-acl` 是空 feature，零成本但影响运行时权限模型，保留现状最安全。

### 3.2 ✅ 建议：移除 `sysinfo`，用标准库重写（−14 / −6 / −6，风险中）

现有两个函数：

```rust
fn server_running(port: u16) -> bool        // main.rs:165  进程名扫描 ‖ 端口可连
fn kill_server_processes()                  // main.rs:179  强杀所有同名进程
```

替换为零依赖版本：

```rust
/// 纯：端口是否接受连接。等价于"server 是否在监听"，也是托盘唯一关心的信号。
fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// 强杀兜底：优先杀自己 spawn 的 Child；外部启动的实例交给系统命令。
fn force_kill_server(child: &mut Option<std::process::Child>) {
    if let Some(c) = child.take() { let _ = c.kill(); }
    #[cfg(windows)]
    let _ = Command::new("taskkill").args(["/F", "/IM", "suwayomi-server.exe"]).output();
    #[cfg(not(windows))]
    let _ = Command::new("pkill").args(["-f", "suwayomi-server"]).output();
}
```

语义差异（需要你确认可接受）：

| 场景 | 现在（sysinfo） | 改后 |
| --- | --- | --- |
| 检测"已有 server 在跑" | 进程名命中 **或** 端口可连 | 只看端口可连 |
| 强杀自己拉起的 server | ✅ | ✅（`Child::kill`） |
| 强杀外部启动、且端口不同的 server | ✅ | ❌（降级为告警） |

因为托盘全程只认 `settings.server_port` 这一个端口，第一种差异在实践中不会触发。第二行差异是唯一的能力回退——但 `stop_server_gracefully` 的强杀只是 6 秒超时后的兜底，本来就是异常路径。

**副作用**：`server_running` 现在是纯函数 `port_open`，`stop_server_gracefully` / `wait_ready` 也跟着变纯（配合 §3.5 的 `poll_until`），顺带解决 §4 里"同一逻辑写两遍"的问题。

### 3.3 ⚠️ 视平台决定：移除 `tauri-plugin-single-instance`（−10 / **−62** / −8，风险中高）

零依赖替代方案（约 40 行，仅用 `std::net`）：

```rust
// 首个实例：绑定 <server_port + 1> 作实例锁，并在线程里 accept
// 第二实例：connect 失败 → 说明已有实例 → 连上去写一个字节 → 退出
// 首个实例：读到字节 → 聚焦 webui/settings 窗口
```

取舍：

| | 保留插件 | 自己实现 |
| --- | --- | --- |
| Linux 体积 | +62 crate | 0 |
| 维护 | 官方维护 | 自己维护 40 行 |
| 行为 | 官方语义（含 D-Bus、argv/cwd 回传） | 只有"聚焦已有窗口" |
| 风险 | 无 | 端口冲突需约定；跨用户/沙箱场景要额外处理 |

建议：**Linux 是主要发布目标 → 值得做；只发 Windows → 保留插件。** 本项目的 plugin 回调（`main.rs:538-548`）本来就只做"聚焦窗口"这一件事，自定义实现的语义覆盖是完整的。

### 3.4 🔧 可选：`png` 从运行时依赖挪到 `build-dependencies`（−0 / −3 / −3，风险低）

现在 `decode_png_icon`（`main.rs:509-530`）在**每次启动时**解码一个编译期就已知的常量 PNG，并且用 `expect` / `panic!` 处理失败——资源文件写错只会在用户机器上崩。

改成构建期解码：

```rust
// build.rs
fn main() {
    tauri_build::build();
    let rgba = decode_tray_png(include_bytes!("../icons/tray.png"));  // 用 build-dependencies 的 png
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("tray_icon.rs");
    std::fs::write(out, format!("pub const RGBA: &[u8] = &{rgba:?};\npub const W: u32 = {w};\npub const H: u32 = {h};\n"))?;
}
```

```rust
// main.rs
include!(concat!(env!("OUT_DIR"), "/tray_icon.rs"));
let tray_icon = tauri::image::Image::new(RGBA, W, H);
```

收益：运行时少一个解码库、少 3 处 panic 点、图标错误变成**编译失败**而不是运行时崩溃。

### 3.5 ❌ 不建议：移除 `open`（只省 1~3）

`open` 5.4 处理了 WSL / Docker / 沙箱路径等边界情况，自己用 `explorer`/`xdg-open` 拼会在这些环境上翻车。省 1 个 crate 不值得。

### 3.6 ❌ 不建议：关掉 `x11` / `dbus`（Linux −9，但语义不明）

`tauri/x11`、`tauri/dbus` 只在 Linux 上生效（`tauri-runtime-wry` → `tao/dbus`）。省 9 个 crate 的代价是可能破坏 Linux 下的托盘图标显示与菜单集成。**收益太小、风险不确定，不做。**

---

## 4. 顺带发现的两个配置一致性问题

### 4.1 `capabilities/default.json` 指向不存在的窗口

```json
"windows": ["main"]     // ← 项目里没有任何 label 为 "main" 的窗口
```

实际发起 `invoke` 的是 `frontend/index.html`（`__TAURI__.core.invoke`，第 66/84/103/111 行），它跑在 label 为 **`settings`** 的窗口里；另一个窗口是 `webui`。

也就是说 ACL 权限被授予了一个不存在的窗口。当前能跑通说明 ACL 匹配没有严格生效，但这是个**定时炸弹**：一旦收紧 capability 或升级 Tauri，设置页的 `get_settings` / `save_settings` / `open_data_dir` 会突然全部被拒。

```json
"windows": ["settings", "webui"]    // 或直接删掉 "windows" 字段（= 对所有窗口生效）
```

### 4.2 `tauri.conf.json` 的 `frontendDist` 是死配置

所有导航都是 CustomProtocol / External，`frontend/` 被内嵌进二进制但从未被访问。配合 §3.1 一起处理。

---

## 5. 执行清单

| 步骤 | 动作 | 省（win/linux/mac） | 验证方式 |
| --- | --- | --- | --- |
| 1 | `tauri` 改 `default-features = false` + 显式 feature 列表 | −6 / −6 / −6 | `cargo build --release` |
| 2 | 删 `frontendDist`，改 `capabilities` 的 `windows` | 0 | 设置页三个 invoke 全通 |
| 3 | 删 `sysinfo`，换 `port_open` + `force_kill_server` | −14 / −6 / −6 | 重启/退出/改端口三条路径 |
| 4 | （Linux 优先）自实现单实例 | −10 / −62 / −8 | 双击第二次 → 聚焦已有窗口 |
| 5 | `png` 挪到 build 期 | −0 / −3 / −3 | 托盘图标正常 + 故意放坏 PNG 应编译失败 |

每一步单独提交，便于二分回退。

---

## 附：成本复现方法

```bash
# 某平台实际编译多少个 crate（不编译，只解析依赖图）
cargo tree --prefix none --target x86_64-pc-windows-msvc   | sort -u | wc -l
cargo tree --prefix none --target x86_64-unknown-linux-gnu | sort -u | wc -l
cargo tree --prefix none --target x86_64-apple-darwin      | sort -u | wc -l

# lock 里哪些包只为某个直接依赖而存在
python docs/migration/dep-cost.py

# 是否有版本重复
cargo tree --duplicates
```

> 注：lock 中 `png 0.17/0.18`、`syn 1/2/3`、`toml 0.8/0.9/1.1`、`windows-sys 0.59/0.60/0.61` 的重复来自 Tauri 生态自身（tray-icon/muda 与 codegen 的选型不同），**无法通过本仓库的配置统一**，不在本次优化范围内。
