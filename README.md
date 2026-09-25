# Suwayomi Tray

Suwayomi-next 的桌面壳（Tauri 2）：驻系统托盘，拉起 `bin/suwayomi-server`，用系统 WebView 打开 WebUI。

- 拆分自 <https://github.com/576576/Suwayomi-next>，独立发布自己的版本
- 产物是裸可执行文件（`suwayomi` / `suwayomi.exe`），不是 `.app` / `.msi` / AppImage
- 许可证：MPL-2.0，与 Suwayomi-next 相同

## 发布布局

桌面壳放在发布包根目录，与 `bin/` 同级：

```
suwayomi.exe            桌面壳（本仓库产出）
bin/suwayomi-server     无头服务端
bin/ext-runtime.jar     扩展沙盒
webui/                  WebUI
```

服务端二进制由 `SUWAYOMI_BIN` 或「exe 同级 `bin/`」定位；数据目录、extensions 目录、日志目录都由桌面壳通过环境变量传给服务端。

## 构建

```bash
SUWAYOMI_TRAY_VERSION=1.0.23 bash build-tray.sh
```

版本号必须是三段 semver（会注入 `tauri.conf.json`，Windows 上还进 PE 版本资源）。不传时按本仓库的提交数推算，规则同 CI。

## 版本号

`versionCode = 提交数 + 1000`，版本名 `1.{提交数/100}.{提交数%100}`。

规则照 Suwayomi-next，只是基线与主版本用本仓自己的：主仓是 `+3000` / `3.y.z`，这里是 `+1000` / `1.y.z`。三个通道共用同一个版本名，差异落在 tag 上：

| 通道 | tag | 触发方式 |
| --- | --- | --- |
| release | `v1.0.23` | 手动 dispatch |
| beta | `v1.0.23-beta.<run_id>` | 手动 dispatch |
| alpha | `1.0.23-alpha.<run_id>` | 推送 main 自动（仅 windows-x64 + linux-x64）／手动 dispatch |

## 发布

CI 与 Suwayomi-next 同款结构：`.github/workflows/build.yml`（可复用构建）+ `.github/workflows/release.yml`（触发器、版本号、发布）。产物是六个桌面 target 的资产：

```
suwayomi-tray-<version>-<target>[.exe]
```

Suwayomi-next 的打包流程按 target 从 Release 取用：正式通道只认非预发布版本，alpha/beta 跟最新构建。
