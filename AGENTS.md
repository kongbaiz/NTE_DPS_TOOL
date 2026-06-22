# AGENTS.md

## 适用范围

本文件适用于整个仓库。除非用户或更深层 `AGENTS.md` 明确覆盖，所有智能体修改、审查、重构和资源维护都必须遵守本文。

## 项目定位

`NTE DPS TOOL` 是 Windows 桌面实时 DPS 工具。主程序使用 Rust 2024 + `eframe/egui`；通过 Npcap 抓取本机 UDP 流量，解析 NTE/UE 网络载荷，并在本地展示队伍、角色、技能、深渊上下行线等统计。`tools/` 是资源维护区，包含 Python 数据管线和 C# CUE4Parse probe；普通运行不依赖这些工具。

## 不可破坏的约束

- 不提交 `target/`、`logs/`、`data/`、`NTE_Assets/`、`tools/external/`、C# `bin/obj`、`.env`、资源导出 AES key、`*.usmap`、完整解包数据或抓包样本。
- 不把资源导出 AES key、授权资源路径、完整载荷、PCAP 内容、用户本机路径写入日志、报告或提交说明。
- `src/encrypted_ini.rs` 中用于 NTE 加密 INI 读写的固定 key 属于长期稳定的协议兼容常量，可保留；不得把资源解包/导出 AES key 或用户授权 key 写入源码。
- 不绕过现有 debug 回放链路：实时抓包、JSON 导出导入、PCAPNG 导入必须尽量复用同一稳定解析流程。
- 不在 `master` 主线重新启用敌方目标识别显示；相关研究只应在 `research/scene-target-identification` 或明确授权的分支进行。
- 不无理由提高 Rust 最低版本、.NET 目标版本、Python 版本或升级 `egui/eframe` 主版本。

## 架构边界

- `src/main.rs`：启动入口，只负责 panic 日志、配置加载、`eframe::NativeOptions` 和 `DpsApp` 创建。
- `src/app.rs`：UI、应用状态和事件编排。不要把新解析算法、资源扫描、网络枚举或长耗时任务塞进帧渲染路径；必要时下沉到独立模块。
- `src/capture.rs`：Npcap FFI、设备枚举、实时抓包、PCAPNG/JSON 导入、原始帧写入和 `EngineEvent` 产生。不得依赖 UI。
- `src/protocol.rs`：UE 传输层位级解析。必须保持纯函数、确定性、无文件系统、无时间、无 UI。
- `src/parser.rs`：伤害载荷、GameplayEffect、技能分类和资源表读取。解析失败应返回 `Option`/`Result`，不得 panic。
- `src/model.rs`：领域模型、序列化结构、战斗聚合和深渊状态。不得加入 UI 或 Win32 API。
- `src/network.rs`：Windows 进程和 TCP 连接检测，仅用于定位 `HTGame.exe` 的本机 IP 与网卡。
- `src/config.rs`：UI 配置加载、保存、迁移与净化。新增字段必须有默认值和兼容旧 JSON 的行为。
- `src/character_editor.rs`：角色表 Debug 编辑器的数据状态、JSON 字段读写和表单校验。不得依赖 egui。
- `src/encrypted_ini.rs`：NTE 加密 INI 的解析、搜索、加解密和记录复用。不得依赖 UI，不得用于资源解包。
- `src/io_util.rs`：原子写文件等通用 I/O 辅助。不得依赖 UI。
- `src/window_attributes.rs`：Win32 窗口圆角、透明度和进程窗口属性处理。平台相关 `unsafe` 必须有 `SAFETY:` 注释。
- `build.rs`：资源内嵌和 Windows 图标。输出必须确定，新增资源路径需保持大小写和分隔符稳定。
- `res/`：稳定运行资源。手工字段优先保留，批量生成必须说明来源。
- `tools/`：离线资源维护工具。不得成为主程序运行时依赖。
- `Dumper-7/`：第三方或生成 SDK 参考区。除非明确更新生成物，不做手工修改。

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

资源工具验证：

```powershell
python -m pip install -r tools/requirements.txt
python tools/nte_asset_pipeline.py --help
dotnet build tools/cue4parse_probe/Cue4ParseProbe.csproj
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
- UI 文案保持中文，术语沿用现有口径：深渊、上下行线、创生花、覆纹、延滞、黯星、浊燃、浸染、盈蓄等不得随意改名。
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
- 更新 `res/` 优先走 `tools/export_nte_res.py` 或 `tools/nte_asset_pipeline.py`；手工编辑只允许小范围修正，并保留已有人工颜色、头像、别名等字段。
- 生成或更新资源时，检查 `asset_report.json`、`asset_manifest.json` 与 README/工具文档是否需要同步。
- 不提交原始客户端容器、授权密钥、usmap、第三方工具目录或中间导出树。
- 新增图片资源要考虑 `build.rs` 会内嵌图片，避免无意义大文件进入二进制。

## Python 工具规范

- Python 工具要求 Python 3.14+；依赖只写入 `tools/pyproject.toml` 和 `tools/requirements.txt`。
- 遵循 PEP 8 基本风格：4 空格缩进，标准库/第三方/本地导入分组，函数和变量使用 `snake_case`。
- CLI 使用 `argparse`，错误信息必须能指导下一步操作。
- 文件路径使用 `pathlib.Path`；输出默认写入 `target/` 或用户显式指定目录。
- JSON 输出使用稳定结构和缩进；不得把资源导出 AES key、完整本机路径或敏感载荷写入报告。

## C# probe 规范

- `tools/cue4parse_probe` 使用 `net10.0`，`ImplicitUsings` 与 `Nullable` 保持启用。
- Probe 只负责资源定位、解密授权输入、CUE4Parse 导出和报告生成；不要加入主程序业务逻辑。
- 命名遵循 .NET 常规：类型/属性/方法 `PascalCase`，局部变量和参数 `camelCase`。
- 资源导出 AES key 只能从环境变量或显式 key 文件读取；不得打印、落盘或进入异常详情。

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
