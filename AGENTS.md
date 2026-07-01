# AGENTS.md

## 适用范围

本文件适用于整个仓库。除非用户或更深层 `AGENTS.md` 明确覆盖，所有智能体修改、审查、重构和资源维护都必须遵守本文。

## 项目定位

`NTE DPS TOOL` 是 Windows 桌面实时 DPS 工具。主程序使用 Rust 2024 + `eframe/egui`；通过 Npcap 抓取本机 UDP 流量，解析 NTE/UE 网络载荷，并在本地展示队伍、角色、技能、深渊上下行线等统计。资源导出和离线维护工具位于独立私有仓库 `kongbaiz/nte-resource-exporter`；普通运行不依赖这些工具。

## 不可破坏的约束

- 不提交 `target/`、`logs/`、`data/`、`NTE_Assets/`、资源工具外部依赖目录、C# `bin/obj`、`.env`、资源导出 AES key、`*.usmap`、完整解包数据或抓包样本。
- 不把资源导出 AES key、授权资源路径、完整载荷、PCAP 内容、用户本机路径写入日志、报告或提交说明。
- `src/support/encrypted_ini.rs` 中用于 NTE 加密 INI 读写的固定 key 属于长期稳定的协议兼容常量，可保留；不得把资源解包/导出 AES key 或用户授权 key 写入源码。
- 不绕过现有 debug 回放链路：实时抓包、JSON 导出导入、PCAPNG 导入必须尽量复用同一稳定解析流程。
- 不在 `master` 主线重新启用敌方目标识别显示；相关研究只应在 `research/scene-target-identification` 或明确授权的分支进行。
- 不无理由提高 Rust 最低版本、.NET 目标版本、Python 版本或升级 `egui/eframe` 主版本。

## 架构边界

源码按职责分目录：`src/app/`（UI）、`src/engine/`（采集→解析→模型管线）、`src/platform/`（Win32/系统集成）、`src/storage/`（持久化与资源读取）、`src/support/`（调试/维护辅助）。`src/main.rs` 只声明这五个目录模块。

- `src/main.rs`：启动入口，只负责 panic 日志、配置加载、`eframe::NativeOptions` 和 `DpsApp` 创建。
- `src/app/`：UI、应用状态和事件编排。`mod.rs` 持有 `DpsApp` 结构、`eframe::App`/`Drop` 实现、共享辅助类型/常量与测试模块；按窗口/页签拆分为子模块（方法子模块 `lifecycle`、`main_view`、`detail_panels`、`console_view` 以 `impl DpsApp` 续写；视图/自由函数子模块 `abyss`、`hit_detail`、`timeline`、`history_ui`、`resources`、`diagnostics_ui`、`editor`、`chrome`、`hud`、`theme`，由 `mod.rs` 以 `pub(crate) use` 再导出，子模块统一 `use super::*` 共享一个扁平 `app` 命名空间）。不要把新解析算法、资源扫描、网络枚举或长耗时任务塞进帧渲染路径；必要时下沉到对应非 UI 模块。
- `src/engine/capture.rs`：Npcap FFI、设备枚举、实时抓包、PCAPNG/JSON 导入、原始帧写入和 `EngineEvent` 产生。不得依赖 UI。
- `src/engine/protocol.rs`：UE 传输层位级解析。必须保持纯函数、确定性、无文件系统、无时间、无 UI。
- `src/engine/parser.rs`：伤害载荷、GameplayEffect、技能分类和资源表读取。解析失败应返回 `Option`/`Result`，不得 panic。
- `src/engine/model.rs`：领域模型、序列化结构、战斗聚合和深渊状态。不得加入 UI 或 Win32 API。
- `src/engine/abyss_data.rs`：深渊怪物静态表、数值与波次预测的纯数据计算。不得依赖 UI。
- `src/platform/network.rs`：Windows 进程和 TCP 连接检测，仅用于定位 `HTGame.exe` 的本机 IP 与网卡。
- `src/platform/window_attributes.rs`：Win32 窗口圆角、透明度和进程窗口属性处理。平台相关 `unsafe` 必须有 `SAFETY:` 注释。
- `src/platform/hotkey.rs`、`src/platform/file_drop.rs`：全局穿透热键与原生文件拖放桥接，平台相关边界同上。
- `src/storage/config.rs`：UI 配置加载、保存、迁移与净化。新增字段必须有默认值和兼容旧 JSON 的行为。
- `src/storage/i18n.rs`：UI 本地化。源码统一以英文字符串为键；`res/languages/<code>.json` 提供“英文键→本地化值”覆盖表（英文无需文件）。`t`/`tf` 做查表与占位符替换，缺失键回退到英文键本身。语言存于 `UiConfig::language`，入口在控制台设置页下拉框。
- `src/storage/history.rs`：本地脱敏战斗历史库的结构、读写、迁移与裁剪。
- `src/storage/io_util.rs`：原子写文件等通用 I/O 辅助。不得依赖 UI。
- `src/storage/resource.rs`：内嵌/外置运行资源的字节与文本读取。
- `src/storage/capture_logs.rs`：`logs/nte_raw_*.pcapng` 原始抓包文件的容量统计与清理（纯文件 I/O，不依赖 UI，删除失败的占用文件跳过）。
- `src/support/character_editor.rs`：角色表 Debug 编辑器的数据状态、JSON 字段读写和表单校验。不得依赖 egui。
- `src/support/encrypted_ini.rs`：NTE 加密 INI 的解析、搜索、加解密和记录复用。不得依赖 UI，不得用于资源解包。
- `src/support/diagnostics.rs`、`src/support/resource_audit.rs`：采集诊断与运行资源覆盖率检查，只消费只读数据，不得阻塞帧。
- `build.rs`：资源内嵌和 Windows 图标。输出必须确定，新增资源路径需保持大小写和分隔符稳定。
- `res/`：稳定运行资源。手工字段优先保留，批量生成必须说明来源。
- 资源导出、CUE4Parse probe 和离线资源维护工具已迁出到独立私有仓库 `kongbaiz/nte-resource-exporter`。主程序仓库不得依赖这些工具运行。
- `Dumper-7/`：本地忽略的第三方或生成 SDK 参考区。主程序和工具不得依赖该目录；除非明确要求引入可审查生成物，不提交该目录内容。

## 开发流程

修改前先判定影响范围：主程序、解析协议、资源数据、工具脚本、构建配置或文档。只改必要文件，保持 diff 小而可审查。

常规验证命令：

```powershell
cargo fmt --check
cargo check
cargo test
```

建议在可行时追加：

```powershell
cargo clippy -- -Dwarnings
cargo run --release
```

依赖真实抓包的诊断测试默认不跑。需要时使用：

```powershell
$env:NTE_TEST_CAPTURE = "<pcapng-path>"
cargo test -- --ignored
```

最终回复必须列出改动文件、已运行命令、未运行命令及原因。

## Rust 规范

- 使用 `rustfmt` 默认格式；不要手工制造对齐或局部风格。
- 命名遵循 Rust 习惯：类型 `PascalCase`，函数/变量/模块 `snake_case`，常量 `SCREAMING_SNAKE_CASE`。
- 优先使用现有 crate：`anyhow` 用于带上下文的 I/O/解析错误；面向 UI 的错误保留可展示的 `String`。
- 生产代码避免 `unwrap()`；`expect()` 只允许用于编译期内嵌资源、测试或不可恢复不变量，并写清语义。
- 所有 `unsafe` 必须最小化、局部化，并附 `SAFETY:` 注释；FFI 资源必须用 RAII/`Drop` 守卫释放。
- 网络包、位偏移、长度计算必须使用边界检查、`checked_*`、`saturating_*` 或显式 guard；禁止先索引再判断。
- 后台线程通过 `crossbeam_channel` 与 UI 通信；不得在 egui 帧内阻塞抓包、文件扫描、JSON 大解析或 CUE4Parse 调用。
- 保持序列化兼容。`Hit` 等导出结构新增字段必须 `#[serde(default)]` 或保证旧 JSON 可导入。
- 新增业务规则必须有测试覆盖；解析规则至少覆盖正常、边界、误判规避三个场景。

## egui/UI 规范

- egui 是 immediate mode：UI 函数只做渲染、轻量状态更新和事件派发。
- 长列表、命中详情和技能汇总必须使用缓存、分页或预算控制；不得移除现有 UI 事件预算常量的保护意图。
- 从后台线程改变状态后，应通过 `Context::request_repaint()` 触发刷新。
- UI 支持中英双语（默认简体中文，可在控制台设置页切换）。新增用户可见文案必须以英文为键调用 `t("...")`/`tf("...", &[...])`，并在 `res/languages/zh-CN.json` 补上对应中文值；不要再硬编码裸中文字面量。游戏内专用名词优先取自 `NTE_Assets` 官方本地化，匹配不到则保留原值；深渊、上下行线、创生花、覆纹、延滞、黯星、浊燃、浸染、盈蓄等术语的既有中文口径不得随意改名（这些是 `zh-CN.json` 的值，不是键）。引擎/模型层仅返回稳定英文键或原始数据，翻译只在 UI 展示点进行。
- UI 改动需说明人工验证范围；涉及窗口透明、置顶、穿透、快捷键或 debug 面板时必须单独列出。

## 抓包与解析规范

- Npcap 通过动态加载使用；新增 FFI 绑定必须保持 C ABI 类型准确，并处理失败返回。
- BPF 过滤器应尽量收窄到目标流量，避免无谓保存全量本机流量。
- 原始帧写入 PCAPNG 是核心 debug 能力；不得静默移除、降级或改变默认路径格式。
- 实时解析、PCAPNG 回放、JSON 回放的字段语义必须一致。
- 解析器应宁可“不识别”也不要制造高置信误判；新增启发式必须记录触发依据。
- 修改 GameplayEffect、技能分类、伤害属性或深渊事件时，必须同步检查 `res/data/skills/`、`res/data/reactions/` 和相关 UI 汇总。

## 资源维护规范

- 普通运行只依赖 Rust、Npcap 和仓库内 `res/`。
- 更新 `res/` 优先走独立资源工具仓库的导出管线；手工编辑只允许小范围修正，并保留已有人工颜色、头像、别名等字段。
- 生成或更新资源时，检查 `asset_report.json`、`asset_manifest.json` 与 README/工具文档是否需要同步。
- 不提交原始客户端容器、授权密钥、usmap、第三方工具目录或中间导出树。
- 新增图片资源要考虑 `build.rs` 会内嵌图片，避免无意义大文件进入二进制。

## 依赖策略

- 新依赖必须说明用途、维护状态、许可证兼容性和替代方案。
- 不为少量代码引入大型框架；不引入异步运行时，除非能证明收益超过复杂度。
- `release` profile 以小体积和稳定发布为目标，保留 `panic = "abort"`、LTO、`opt-level = "z"` 等意图。
- 升级 `eframe/egui`、`windows-sys`、Npcap 相关实现或 CUE4Parse 时，必须检查 API 变更、UI 行为和 Windows 兼容性。

## 提交与回复规范

- 提交信息使用动词开头，说明实际改动，例如 `Fix damage parser boundary check`。
- 每次改动后给出：改动摘要、影响范围、验证结果、风险点。
- 若未能运行某项验证，直接说明原因，不得声称已验证。
- 不做无关重排、批量格式化或风格清洗；格式化只限被改动语言的标准工具。
