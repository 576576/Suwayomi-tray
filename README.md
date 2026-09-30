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

## 端口

托盘设置里的端口留空 = 自动：启动前试绑默认端口（4567），被占用或被系统保留（Windows 的
Hyper-V 动态保留区会让 bind 报 10013）时向上顺延。填了具体端口就照用，不可用时拒绝启动并在
`tray.log` 里说明 —— 按端口配好的防火墙与端口转发不该因为悄悄换端口而失效。

沙盒（`bin/ext-runtime.jar`）的端口由托盘一并选定（默认从 4568 起嗅探），恒不等于服务端端口；
服务端在沙盒端口上监听时，沙盒启动会先清掉占用者，也就是把服务端杀掉。

「服务器地址」留空 = 打开本机的 `http://127.0.0.1:{实际端口}`；填了就用它（只给主机名时补上
实际端口），用于指向局域网或远程的 server。该地址同时被加进 WebView 的允许源 —— 否则 WebUI
自身的跳转会被当成外部链接丢给系统浏览器。

## 构建

```bash
SUWAYOMI_TRAY_VERSION=1.0.23 bash build-tray.sh
```

版本号必须是三段 semver（会注入 `tauri.conf.json`，Windows 上还进 PE 版本资源）。不传时按本仓库的提交数推算，规则同 CI。

注入是**临时**的：脚本编译前改写 `tauri.conf.json`、编译后还原。仓库里这个字段应保持与 `Cargo.toml` 的 `package.version` 一致，`.githooks/pre-commit` 会拦住注入残留（见「提交前钩子」）。

## 提交前钩子

`tauri.conf.json` 的 `version` 是构建期注入位。本地构建期间这个字段会短暂变成真实版本号，若此时提交 —— GitHub Desktop 会把工作区里所有改动一并列出 —— 注入结果就进了仓库（`0358f3a` 即此，已 revert）。

`.githooks/pre-commit` 校验它与 `Cargo.toml` 的 `package.version` 相等，不等即视为注入残留、拒绝提交。只比对这两个版本号，`sh` + `sed`/`awk`，无额外依赖。启用（每个克隆做一次）：

```bash
git config core.hooksPath .githooks
```

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
