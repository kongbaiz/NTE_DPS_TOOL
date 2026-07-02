# AGENTS.md

## 0. 最小行为守则（动手前必读）

本节是全文的强制摘要，供任何智能体（无论能力强弱）在动手前快速对齐；与正文冲突时以正文为准。

1. **先定位规则再动手**：改任何文件前，先在 §4「架构边界」找到该文件的条目并遵守其约束；要新建文件，先按 §4 的归属表确定目录。
2. **只做被要求的事**：只改与当前任务直接相关的文件。发现无关问题（bug、坏味道、过期注释）只记录并在回复中报告，不顺手修。
3. **改完必验证**：`cargo fmt --check`、`cargo check`、`cargo test` 三条命令全部通过才算完成；跑不了的要在回复里写明原因，不得声称已验证。
4. **默认禁止清单**（除非用户当次明确要求）：`git commit` / `git push`；新增、升级、删除依赖；修改 `Cargo.toml` 的 `[profile]` / `[patch]` 段；修改 `vendor/`；拆分、合并、重命名模块文件；批量格式化或风格清洗。
5. **UI 验证交给用户**：禁止自行启动程序、模拟点击或截图来"验证"UI 改动；改完列出人工验证清单，由用户操作并反馈结果。
6. **文案必须走 i18n**：新增用户可见文案一律 `t("English key")` / `tf(...)`，并在 `res/languages/zh-CN.json` 补中文值；禁止在源码硬编码中文字面量。
7. **校验只做在信任边界**：解析外部输入（网络字节、文件、FFI）时做一次完整校验；边界之内禁止防御性代码（详见 §5）。
8. **不确定就停**：任务与本文约束冲突、或存在多种合理理解时，停下来向用户说明歧义和备选方案；禁止凭猜测扩大改动范围。
9. **回复有固定格式**：最终回复必须包含改动文件、验证命令结果、未运行项及原因、需人工验证的点（见 §13）。

## 1. 适用范围与优先级

- 本文适用于整个仓库的一切修改、审查、重构和资源维护。
- 优先级：用户当次明确指令 > 更深层目录的 `AGENTS.md` > 本文。
- 本文的**规范性条款**（"必须 / 禁止"）始终有效；若任务要求违反，先停下询问，不得自行取舍。
- 本文的**描述性内容**（架构、文件职责）与代码现状不符时，以代码为准，并在回复中报告文档偏差；不要为了让代码符合文档而扩大改动。

## 2. 项目定位

`NTE DPS TOOL` 是 Windows 桌面实时 DPS 工具。主程序使用 Rust 2024 + `eframe/egui`（渲染后端固定 `wgpu`，见 §10）；通过 Npcap 抓取本机 UDP 流量，解析 NTE/UE 网络载荷，并在本地展示队伍、角色、技能、深渊上下行线等统计。资源导出和离线维护工具位于独立私有仓库 `kongbaiz/nte-resource-exporter`；普通运行不依赖这些工具。

## 3. 不可破坏的约束

- 不提交 `target/`、`logs/`、`data/`、`NTE_Assets/`、`nte-resource-exporter/`、`Dumper-7/`、`tools/`、C# `bin/obj`、`.env`、资源导出 AES key、`*.usmap`、完整解包数据或抓包样本。这些根目录条目是本地参考/工具区，主程序不得依赖它们运行。
- 不把资源导出 AES key、授权资源路径、完整载荷、PCAP 内容、用户本机路径写入日志、报告或提交说明。
- `src/support/encrypted_ini.rs` 中用于 NTE 加密 INI 读写的固定 key 属于长期稳定的协议兼容常量，可保留；不得把资源解包/导出 AES key 或用户授权 key 写入源码。
- 不绕过现有 debug 回放链路：实时抓包、JSON 导出导入、PCAPNG 导入必须尽量复用同一稳定解析流程。
- 不在 `master` 主线重新启用敌方目标识别显示；相关研究只应在 `research/scene-target-identification` 或明确授权的分支进行。
- 不无理由提高 Rust 最低版本、.NET 目标版本、Python 版本或升级 `egui/eframe` 主版本（升级前置条件见 §10、§11）。

## 4. 架构边界

源码按职责分五个目录，`src/main.rs` 只声明这五个模块。新代码先按下表定归属，再看对应文件的具体约束：

| 新代码的性质 | 归属 | 绝对禁止 |
| --- | --- | --- |
| 界面渲染、状态展示、事件派发 | `src/app/` 对应视图子模块 | 在帧渲染路径做解析、扫描、长耗时任务 |
| 抓包、设备枚举、PCAPNG/JSON 导入 | `src/engine/capture.rs` | 依赖 UI |
| UE 传输层位级解析 | `src/engine/protocol.rs` | 文件系统、时间、UI、非确定性 |
| 伤害/技能/GameplayEffect 语义解析 | `src/engine/parser.rs` | panic（失败返回 `Option`/`Result`） |
| 领域模型、战斗聚合、序列化结构 | `src/engine/model.rs` | UI、Win32 API |
| 深渊静态表与波次预测计算 | `src/engine/abyss_data.rs` | UI |
| Win32/系统集成（窗口、热键、网络探测、拖放） | `src/platform/` | 业务逻辑 |
| 配置、历史库、资源读取、i18n、通用 I/O | `src/storage/` | UI |
| Debug/维护辅助（编辑器状态、诊断、加密 INI） | `src/support/` | 依赖 egui、阻塞帧 |

各文件职责：

- `src/main.rs`：启动入口，只负责 panic 日志、配置加载、`eframe::NativeOptions` 和 `DpsApp` 创建。
- `src/app/`：UI、应用状态和事件编排。`mod.rs` 持有 `DpsApp` 结构、`eframe::App`/`Drop` 实现、共享辅助类型/常量与测试模块；按窗口/页签拆分为子模块（方法子模块 `lifecycle`、`main_view`、`detail_panels`、`console_view` 以 `impl DpsApp` 续写；视图/自由函数子模块 `abyss`、`hit_detail`、`timeline`、`history_ui`、`resources`、`diagnostics_ui`、`editor`、`chrome`、`hud`、`theme`，由 `mod.rs` 以 `pub(crate) use` 再导出，子模块统一 `use super::*` 共享一个扁平 `app` 命名空间）。
- `src/engine/capture.rs`：Npcap FFI、设备枚举、实时抓包、PCAPNG/JSON 导入、原始帧写入和 `EngineEvent` 产生。
- `src/engine/protocol.rs`：UE 传输层位级解析。必须保持纯函数、确定性。
- `src/engine/parser.rs`：伤害载荷、GameplayEffect、技能分类和资源表读取。
- `src/engine/model.rs`：领域模型、序列化结构、战斗聚合和深渊状态。
- `src/engine/abyss_data.rs`：深渊怪物静态表、数值与波次预测的纯数据计算。
- `src/platform/network.rs`：Windows 进程和 TCP 连接检测，仅用于定位 `HTGame.exe` 的本机 IP 与网卡。
- `src/platform/window_attributes.rs`：Win32 窗口圆角、透明度和进程窗口属性处理。
- `src/platform/hotkey.rs`、`src/platform/file_drop.rs`：全局穿透热键与原生文件拖放桥接。
- `src/storage/config.rs`：UI 配置加载、保存、迁移与净化。新增字段必须有默认值和兼容旧 JSON 的行为。
- `src/storage/i18n.rs`：UI 本地化。源码统一以英文字符串为键；`res/languages/<code>.json` 提供"英文键→本地化值"覆盖表（英文无需文件）。`t`/`tf` 做查表与占位符替换，缺失键回退到英文键本身。语言存于 `UiConfig::language`，入口在控制台设置页下拉框。
- `src/storage/history.rs`：本地脱敏战斗历史库的结构、读写、迁移与裁剪。
- `src/storage/io_util.rs`：原子写文件等通用 I/O 辅助。
- `src/storage/resource.rs`：内嵌/外置运行资源的字节与文本读取。
- `src/storage/capture_logs.rs`：`logs/nte_raw_*.pcapng` 原始抓包文件的容量统计与清理（纯文件 I/O，删除失败的占用文件跳过）。
- `src/support/character_editor.rs`：角色表 Debug 编辑器的数据状态、JSON 字段读写和表单校验。
- `src/support/encrypted_ini.rs`：NTE 加密 INI 的解析、搜索、加解密和记录复用。不得用于资源解包。
- `src/support/diagnostics.rs`、`src/support/resource_audit.rs`：采集诊断与运行资源覆盖率检查，只消费只读数据。
- `build.rs`：资源内嵌和 Windows 图标。输出必须确定，新增资源路径需保持大小写和分隔符稳定。
- `res/`：稳定运行资源。手工字段优先保留，批量生成必须说明来源。
- `vendor/egui-winit-0.34.3/`：本地 fork 依赖，仅为 §10 透明背景修复而存在，见 §11。
- 资源导出、CUE4Parse probe 和离线资源维护工具已迁出到独立私有仓库 `kongbaiz/nte-resource-exporter`。主程序仓库不得依赖这些工具运行。
- `Dumper-7/`：本地忽略的第三方或生成 SDK 参考区。主程序和工具不得依赖该目录。

模块结构本身（文件的拆分、合并、重命名、re-export 布局）属于用户决策；智能体不得为了"整洁"自行调整，即使某个文件很大（`capture.rs` 近 5000 行是已知且接受的现状）。

## 5. 代码风格：简洁优先，按信任边界校验

本节回答"这段代码要不要检查、要不要抽象"。原则：**在保证功能正确的前提下，用最少的概念和分支表达意图；校验集中在信任边界做一次，边界之内不写防御性代码。**

### 5.1 信任边界（必须校验的地方）

以下入口处理的是不可信数据，必须完整校验，失败返回 `Option`/`Result`，不得 panic：

- 网络字节流：`engine/protocol.rs`、`engine/capture.rs` 的帧与载荷解析。位偏移、长度、索引计算必须用 `checked_*`、`saturating_*` 或显式 guard，禁止先索引再判断。
- 外部文件：PCAPNG/JSON 导入、加密 INI、`res/` 资源、用户配置与历史库的读取和反序列化。
- FFI 与系统调用返回值：Npcap、Win32 API。

这是边界校验，不是防御性编程；两者的区别就在数据来源。

### 5.2 边界之内（禁止防御性代码）

数据一旦通过上述入口进入 model/UI 层即视为可信。禁止：

- 对内部函数参数做"以防万一"的 None 检查、空集合检查、范围检查；
- 用 `unwrap_or_default()`、`.ok()`、`if let` 静默吞掉"按当前逻辑不可能发生"的错误——这是把 bug 藏起来，不是修掉；
- 同一条数据在多层重复做相同校验；
- 只记日志就继续执行的空兜底分支。

内部不变量按以下优先级表达：

1. 调整类型让非法状态无法表示（枚举替代标志位组合、新类型、构造时保证非空）；
2. `match` 穷举，不写多余的 `_ =>` 兜底；
3. `expect("<说明为何不可能失败>")`——不变量被破坏时应当立刻 panic 暴露 bug，而不是静默降级。生产代码仍避免裸 `unwrap()`；`expect()` 只用于编译期内嵌资源、测试或上述不变量。

### 5.3 简洁性硬规则

- 只实现当前任务需要的功能。禁止预留参数、预留配置项、"未来可能用到"的分支或钩子。
- 单一调用点不引入封装层；单一实现不定义 trait；两处重复不足以立刻抽象，出现第三处再考虑。
- 优先 early return 降低嵌套；迭代器链与手写循环选可读性更好的那个。
- 替换逻辑时删除旧代码：不注释掉、不留 `_old`/`_v2` 命名、不保留新旧双路径开关。
- 注释只写代码表达不了的约束（协议位布局、Windows 特殊行为、`SAFETY:`、数值口径来源）；禁止叙述"下一行在做什么"或"此处改了什么"的注释。
- 修 bug 先定位根因并在根因处修复；禁止在症状处加 guard 或 fallback 掩盖。
- 新代码的命名、注释密度、错误处理方式与所在文件保持一致；不要引入该文件没有的新范式。

## 6. Rust 规范

- 使用 `rustfmt` 默认格式；不要手工制造对齐或局部风格。
- 命名遵循 Rust 习惯：类型 `PascalCase`，函数/变量/模块 `snake_case`，常量 `SCREAMING_SNAKE_CASE`。
- 错误处理用现有 crate：`anyhow` 用于带上下文的 I/O/解析错误；面向 UI 的错误保留可展示的 `String`。
- 所有 `unsafe` 必须最小化、局部化，并附 `SAFETY:` 注释；FFI 资源必须用 RAII/`Drop` 守卫释放。
- 后台线程通过 `crossbeam_channel` 与 UI 通信；不得在 egui 帧内阻塞抓包、文件扫描或 JSON 大解析。
- 保持序列化兼容。`Hit` 等导出结构新增字段必须 `#[serde(default)]` 或保证旧 JSON 可导入。
- 测试写在同文件的 `#[cfg(test)] mod tests`（与现有各模块惯例一致）。新增业务规则必须有测试覆盖；解析规则至少覆盖正常、边界、误判规避三个场景。

## 7. egui/UI 规范

- egui 是 immediate mode：UI 函数只做渲染、轻量状态更新和事件派发。
- 长列表、命中详情和技能汇总必须使用缓存、分页或预算控制；不得移除现有 UI 事件预算常量的保护意图。
- 从后台线程改变状态后，应通过 `Context::request_repaint()` 触发刷新。
- UI 支持中英双语（默认简体中文，可在控制台设置页切换）。新增用户可见文案必须以英文为键调用 `t("...")`/`tf("...", &[...])`，并在 `res/languages/zh-CN.json` 补上对应中文值；不要再硬编码裸中文字面量。游戏内专用名词优先取自 `NTE_Assets` 官方本地化，匹配不到则保留原值；深渊、上下行线、创生花、覆纹、延滞、黯星、浊燃、浸染、盈蓄等术语的既有中文口径不得随意改名（这些是 `zh-CN.json` 的值，不是键）。引擎/模型层仅返回稳定英文键或原始数据，翻译只在 UI 展示点进行。
- **UI 改动的验证由用户人工执行**：智能体不得自行启动程序、模拟点击、注入输入或截图来确认效果。改完后在回复中列出人工验证清单（入口位置、操作步骤、预期表现），涉及窗口透明、置顶、穿透、快捷键或 debug 面板时必须单独列出。

## 8. 抓包与解析规范

- Npcap 通过动态加载使用；新增 FFI 绑定必须保持 C ABI 类型准确，并处理失败返回。
- BPF 过滤器应尽量收窄到目标流量，避免无谓保存全量本机流量。
- 原始帧写入 PCAPNG 是核心 debug 能力；不得静默移除、降级或改变默认路径格式。
- 实时解析、PCAPNG 回放、JSON 回放的字段语义必须一致。
- 解析器宁可"不识别"也不要制造高置信误判；新增启发式必须记录触发依据。
- 修改 GameplayEffect、技能分类、伤害属性或深渊事件时，必须同步检查 `res/data/skills/`、`res/data/reactions/` 和相关 UI 汇总。

## 9. 资源维护规范

- 普通运行只依赖 Rust、Npcap 和仓库内 `res/`。
- 更新 `res/` 优先走独立资源工具仓库的导出管线；手工编辑只允许小范围修正，并保留已有人工颜色、头像、别名等字段。
- 生成或更新资源时，检查 `asset_report.json`、`asset_manifest.json` 与 README/工具文档是否需要同步。
- 不提交原始客户端容器、授权密钥、usmap、第三方工具目录或中间导出树。
- 新增图片资源要考虑 `build.rs` 会内嵌图片，避免无意义大文件进入二进制。

## 10. Windows 渲染与透明度（wgpu / DirectComposition）

主程序渲染后端固定为 `wgpu`（非 `glow`）：借边框透明窗口在 OpenGL 下，从窗口角落斜向快速拖拽缩放时 NVIDIA 驱动会直接丢失 GL 上下文导致进程崩溃（无 panic 日志，`egui` #4061 / #5460，上游未修）。`wgpu` 完全避开这条驱动路径，代价是 Windows 下默认拿不到真正的窗口透明——见下。

- **HUD 透明背景变黑问题**：`wgpu` 在 Windows 上创建的普通 HWND swapchain（D3D12、Vulkan 都一样）永远只能协商到 `CompositeAlphaMode::Opaque`，"透明"窗口会画成纯黑（`egui` #4451、wgpu #1375 / #7108，均为上游未修的已知限制；已实测切到 Vulkan 后端同样无效）。修复由两部分组成，**缺一不可**：
  1. `src/main.rs` 的 `wgpu_options_with_transparent_dx12()`：把后端固定为 `Backends::DX12`，并设置 `Dx12BackendOptions.presentation_system = Dx12SwapchainKind::DxgiFromVisual`（wgpu 27+ 内建的 DirectComposition 支持，自动创建 `IDCompositionVisual` 承载 swapchain）。
  2. `vendor/egui-winit-0.34.3/`：`egui-winit` 0.34.3 的本地 fork，仅打了一处补丁——透明窗口额外设置 `WindowAttributesExtWindows::with_no_redirection_bitmap(true)`（补丁位置：`create_winit_window_attributes` 里的 `#[cfg(target_os = "windows")]` 分支）。没有这一步，HWND 自带的默认 GDI 重定向表面依然存在，会在 DirectComposition 可视化层周围/下方露出一块带原生标题栏的白色系统窗口。补丁说明见该目录下 `PATCH_NOTES.md`；`Cargo.toml` 用 `[patch.crates-io]` 接管这份 fork。
  - 只设置 `DxgiFromVisual` 而不打 `no_redirection_bitmap` 补丁：透明生效但会叠加一层白色系统窗口残影。只打补丁而不设 `DxgiFromVisual`：完全不透明。两处均已单独实测验证过上述失败现象。
  - 升级 `eframe`/`egui-winit` 版本前，必须先对照新版本重新核对/重打 `no_redirection_bitmap` 补丁，否则会静默回归黑屏；这也是"不无理由升级 `egui/eframe` 主版本"约束的具体原因之一。
  - 若上游 `egui-winit`/`eframe` 未来把这个 Windows 专属窗口属性开放到公共 API（关注 https://github.com/emilk/egui/issues/4451 ），应移除 `vendor/` 目录和 `Cargo.toml` 里的 `[patch.crates-io]`，改用官方接口。
  - 排查此类问题时，`env_logger::init()`（已在 `main.rs` 里接入）配合 `RUST_LOG=warn,egui_wgpu=debug` 运行可以看到 `egui_wgpu::winit` 关于 `CompositeAlphaMode` 协商失败的告警，是判断修复是否生效的直接依据。

## 11. 依赖策略

- `vendor/egui-winit-0.34.3/` 是唯一例外的本地 fork 依赖，仅为 §10 所述的透明背景修复而存在；不要以此为先例引入其他 vendored/fork 依赖，也不要在无关改动里顺手清理它。
- 新依赖必须说明用途、维护状态、许可证兼容性和替代方案，并经用户确认后才能加入。
- 不为少量代码引入大型框架；不引入异步运行时，除非能证明收益超过复杂度。
- `release` profile 以小体积和稳定发布为目标，保留 `panic = "abort"`、LTO、`opt-level = "z"` 等意图。
- 升级 `eframe/egui`、`windows-sys` 或 Npcap 相关实现时，必须检查 API 变更、UI 行为和 Windows 兼容性；`eframe/egui-winit` 升级另见 §10 的补丁前置条件。

## 12. 开发流程与验证

修改前先判定影响范围：主程序、解析协议、资源数据、构建配置或文档。只改必要文件，保持 diff 小而可审查。

每次改动后必须运行并通过：

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

收尾自查清单（回复前逐项核对）：

- [ ] diff 里没有与任务无关的文件、格式化噪音或顺手修改；
- [ ] 新增文案全部走 `t()`/`tf()` 且 `zh-CN.json` 已补值；
- [ ] 没有新增防御性检查、预留接口或被注释掉的旧代码（§5）；
- [ ] 序列化结构改动保持旧 JSON 兼容（§6）；
- [ ] 三条必跑命令的实际结果已记录，失败或未跑的写明原因。

## 13. 提交与回复规范

- 未经用户明确要求不执行 `git commit` / `git push`。被要求提交时：提交信息动词开头、描述实际改动（例：`Fix damage parser boundary check`），不夹带无关文件，不用 `git add -A` 盲加。
- 最终回复固定包含五项：
  1. 改动摘要与影响范围；
  2. 改动文件清单；
  3. 已运行的验证命令与结果；
  4. 未运行的验证及原因（不得声称已验证）；
  5. 需人工验证的点（UI 改动必填，按 §7 列出操作步骤与预期表现）。
- 不做无关重排、批量格式化或风格清洗；格式化只限被改动语言的标准工具。
