# agent 文档

给**维护者与 AI** 看的文档：现在是什么样、改的时候要注意什么。

面向用户的说明在根 [`README.md`](../../README.md)（发布布局、端口、构建、提交前钩子、版本号、发布），不放这里。

| 文件 | 内容 |
|---|---|
| `01-dependency-simplification.md` | 依赖精简评估（2026-09-26）：直接依赖与 lock 的构成、Tauri 默认 feature 与两个可自写库的代价、三个方案与各自收益、已落地的范围 |
| `02-functional-style-review.md` | 代码风格评估（2026-09-26）：`src/main.rs` 由单文件拆成模块、`AppState` actor 化、`Logger` 注入、托盘图标改为构建期解码；已落地的范围 |
| `dep-cost.py` | `01` 的复核脚本：重跑可复现「独占包」那组数字 |

这两份是**记录型**文档（评估日期 + 已落地范围），结论不再改写；新的评估另开文件。

## 跨仓文档

本项目由四个各自发版的仓库组成，其余三个的维护者文档在自己的仓库里：

| 仓库 | 文档索引 |
|---|---|
| Suwayomi-next（Rust server） | https://github.com/576576/Suwayomi-next/blob/main/docs/agent/README.md |
| Suwayomi-ext-runtime（扩展沙盒） | https://github.com/576576/Suwayomi-ext-runtime/blob/main/docs/agent/README.md |
| Suwayomi-WebUI（默认前端） | https://github.com/576576/Suwayomi-WebUI/blob/main/docs/agent/README.md |

引用对方的文档一律写**远端地址**，不写相对路径：四个仓各自发版，本地目录布局不保证同时在场。

## 维护约定

- **只写「是什么」和「不这么做会怎么错」**。决策过程与试错史留在 commit message 里，不进这里。
- 代码里放不下的**理由**写到这里；代码注释只留结论。
- 与代码现状对不上的口径当场改掉 —— 过时的事实比没有文档更糟。
