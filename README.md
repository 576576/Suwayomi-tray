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
SUWAYOMI_TRAY_VERSION=1.4.0 bash build-tray.sh
```

版本号必须是三段 semver（Windows 上会写进 PE 版本资源）。不传时回退到本仓库的提交数。

## 发布

推 tag `v1.4.0` 即触发 Release，产出六个桌面 target 的资产：

```
suwayomi-tray-<version>-<target>[.exe]
```

Suwayomi-next 的打包流程按 target 从最新 Release 取用。
