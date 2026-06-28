# NTE DPS TOOL 后续功能规划

本文面向后续开发排期，不是承诺清单。所有功能都应遵守 `AGENTS.md` 的架构边界：实时抓包、PCAPNG 回放、JSON 回放尽量复用同一解析流程；主线不重新启用敌方目标识别显示；不保存原始抓包、完整载荷、本机路径或资源导出密钥到用户可分享的摘要文件。

## 规划原则

- 优先做“复用现有 `Hit` / `CombatState` / `AbyssRunState` 数据”的功能，少碰协议解析核心。
- UI 功能先放在已有 Console 或独立 viewport 中，避免主窗口越来越拥挤。
- 长列表继续使用缓存、分页或虚拟滚动；不要在 egui 帧内做大 JSON 解析、资源扫描或历史库全量统计。
- 新增导出或历史记录只保存脱敏摘要，不保存 PCAPNG、payload hex、decoded text、用户本机路径或完整客户端资源路径。
- 新增配置字段必须 `#[serde(default)]`，并在 `UiConfig::sanitized()` 中处理异常值。

## 参考实现与 UI 借鉴

- `egui_plot` 提供 egui 内的折线、标记线、图例、坐标格式化等能力，适合 DPS 曲线、时停区间和深渊上下行线切换标记。当前项目没有该依赖，若引入必须说明用途、许可证和替代方案；也可以先用 `egui::Painter` 手绘轻量曲线。参考：https://docs.rs/egui_plot
- `egui::ScrollArea::show_rows` 是现有命中明细已经在用的方向，适合数千行命中或历史记录的虚拟化显示，避免逐行布局所有元素。参考：https://docs.rs/egui/latest/egui/containers/scroll_area/struct.ScrollArea.html
- `egui_extras::TableBuilder` 适合固定表头、可滚动 body、列宽控制的统计表。若未来引入 `egui_extras`，需先评估依赖体积和版本与 `eframe/egui` 的兼容性；短期可以延续现有自绘表头和 `ScrollArea::show_rows`。参考：https://docs.rs/egui_extras/latest/egui_extras/struct.TableBuilder.html
- MangoHud 这类游戏性能 Overlay 的 UI 值得借鉴：高密度指标、小型曲线、可开关模块、透明背景、实时性能数据不打断游戏画面。NTE DPS TOOL 的 HUD 可以借鉴“少文字、强数值、可隐藏模块”的思路，但不要复制其平台实现。参考：https://github.com/flightlessmango/MangoHud

## 第一阶段：分析体验增强

### 1. 战斗时间轴 / DPS 曲线

目标：把一次战斗按固定时间窗口聚合，显示队伍 DPS、角色 DPS、总伤害增长、时停区间、深渊上下行线切换点和通关/离开事件。用户能快速看出爆发期、真空期和异常归因时段。

调整方向：

- 新增纯聚合逻辑，优先放在 `src/model.rs`。建议新增 `TimelineBucket`、`TimelineSeries`、`TimelineMarker`，输入 `&[Hit]`、时停事件或已合并时停区间，输出按秒分桶的数据。
- 如果 `TimeStopTracker` 仍是私有实现，可先增加只读摘要方法，例如 `time_stop_intervals_between(start, end)`，不要把 tracker 暴露给 UI 直接改。
- `PartyCombatState` 和 `CombatState` 都需要能生成时间轴；深渊上下行线使用 `AbyssRunState::half()` 的 hit 集合生成。
- 在 `src/app.rs` 中给 Console 增加一个页签，例如 `ConsoleTab::Timeline`，或者在队伍战斗明细窗口顶部增加小型曲线。
- UI 初版建议用 `egui::Painter` 手绘：固定高度 120-160px，横轴为战斗时间，纵轴为 DPS 或伤害，角色曲线使用角色颜色，时停用半透明竖向背景带，深渊事件用竖线和短标签。
- 如果后续引入 `egui_plot`，改动集中在 `src/app.rs` 绘图函数和 `Cargo.toml`，聚合模型不需要变化。

涉及文件：

- `src/model.rs`：新增时间轴聚合结构和测试；避免 UI 类型进入模型层。
- `src/app.rs`：新增页签状态、缓存 key、绘图函数；不要在每帧重建大 Vec，按 `hits_generation` 刷新。
- `src/config.rs`：如需保存曲线显示选项，新增 `timeline_bucket_seconds`、`timeline_show_roles` 等字段，并给默认值。
- `README.md`：功能稳定后补充说明和截图。

注意事项：

- 分桶窗口建议先固定为 1s，后续再做 0.5s / 2s 可选。
- `timestamp` 可能不是从 0 开始，UI 展示要统一归一化到战斗开始后的秒数。
- 时停扣除模式会影响 DPS 口径，UI 需要明确使用当前 `DpsTimeMode`。
- 命中记录有上限裁剪，历史曲线只对当前保留窗口负责，不要声称是完整战斗，除非历史摘要另存。
- 测试至少覆盖：空命中、单命中、多角色同秒、多桶边界、时停区间覆盖。

### 2. 技能占比与未知归因诊断页

目标：把已有的 `ability_name`、`damage_name`、`attack_type`、`gameplay_effect_name` 组织成可读的技能构成视图，同时列出未知技能、未知方向、未知角色、未映射 GameplayEffect，帮助用户判断输出结构，也帮助维护资源映射。

调整方向：

- 在 `src/model.rs` 增加聚合函数，例如 `summarize_skill_breakdown(hits, char_id/filter)`，输出按角色、技能分类、伤害名称、GE 名称的层级统计。
- 优先用现有字段：`attack_type` 作为一层分类，`ability_name` / `damage_name` 作为展示名，`gameplay_effect_index` / `gameplay_effect_name` 用于诊断缺口。
- 在 `src/app.rs` 的 Console 增加“技能”或“归因”页签。顶部显示全队技能占比，点击角色后显示角色技能明细。
- UI 可以借鉴性能面板的高密度读数：左侧角色列表，右侧技能条形图；每行展示技能名、伤害、占比、命中数、均伤、是否 follow-up。
- 未知项不要混在普通技能里，应单独放“待映射”区，支持复制 GE index/name，方便维护 `res/data/skills/`。

涉及文件：

- `src/model.rs`：新增技能聚合结构，例如 `SkillBreakdownRow`、`UnknownAttributionSummary`。
- `src/app.rs`：新增 Console 页签、技能聚合缓存、条形图/表格绘制函数。
- `src/parser.rs`：只有在发现分类规则缺口时才改；不要为 UI 展示硬塞解析规则。
- `res/data/skills/gameplay_effect_mapping.json`、`res/data/skills/skill_damage.json`：只在确认资源映射缺失时更新。
- `src/capture.rs` 测试：如果新增分类规则，必须覆盖正常、边界、误判规避场景。

注意事项：

- “未知”是诊断结果，不等于错误。文案建议用“待映射”或“未归类”，避免用户误解为解析失败。
- 对 `follow_up_damage` 要独立展示或合并展示但注明；否则覆纹等后续伤害会和原命中混淆。
- 技能名过长时必须截断并 tooltip 展示全名，避免表格错位。
- 不要为了技能页引入敌方目标显示。

### 3. 回放解析质量报告

目标：PCAPNG / JSON 导入后，自动生成一页“本次解析质量”报告：包数、命中数、输出/受击/unknown 方向比例、未知角色数量、未知技能数量、时停事件数、深渊事件数、服务端伤害校准命中数等。

调整方向：

- 把当前散落在 Debug 诊断和状态栏里的统计整理成一个 `CaptureQualitySummary`。
- 在 `src/capture.rs` 的导入/实时解析路径中，不要改变 `EngineEvent` 语义；质量报告优先从已进入 `CombatState` 的数据和现有 `PacketDebug` 汇总生成。
- 在 `src/app.rs` 的“诊断”页增加报告区域；如果是导入任务结束，状态栏提示“可在诊断页查看解析质量”。
- 报告支持复制脱敏文本，但不能包含 payload preview、payload hex、decoded text、IP、端口或本机路径。

涉及文件：

- `src/model.rs`：新增 `CaptureQualitySummary` 及从 `CombatState` 构建的函数。
- `src/app.rs`：诊断页新增报告卡片和复制按钮。
- `src/capture.rs`：若需要新增计数事件，必须保证实时、PCAPNG、JSON 三条链路一致。
- `README.md`：补充用户提交问题时推荐附带“脱敏解析质量报告”。

注意事项：

- 报告应显示“统计来源”：实时抓包、PCAPNG 回放或 JSON 回放。
- 不能把 `logs/nte_raw_*.pcapng` 路径写入可复制文本；界面本地显示可以保留现有路径提示。
- 测试重点是脱敏输出不包含敏感字段。

## 第二阶段：长期使用价值

### 4. 本地战斗记录库

目标：保存每次战斗的脱敏摘要，用于历史比较、配队对比和深渊预测。记录库只保存统计结果，不保存原始包、payload、decoded text、本机路径或资源授权信息。

调整方向：

- 新增模块 `src/history.rs`，负责历史记录结构、加载、保存、迁移和裁剪。
- 历史目录建议放在 `%LOCALAPPDATA%\NTE DPS Tool\history\`，不要写入仓库 `logs/` 或 `data/`。
- 记录结构建议包含：版本、保存时间、来源类型、战斗时长、DPS 时间口径、总伤害、总受击、角色摘要、技能摘要、深渊站点/上下行线摘要、解析质量摘要。
- 在 `src/app.rs` Console 增加“历史”页：列表 + 详情。列表用虚拟滚动；详情复用技能占比和时间轴组件。
- 增加“保存本次摘要”按钮，初期不要自动保存，避免用户不知情地产生本地数据。

涉及文件：

- `src/history.rs`：新增文件，处理 JSON schema、读写和错误展示字符串。
- `src/model.rs`：提供从当前状态构建 `CombatSessionSummary` 的纯函数。
- `src/app.rs`：新增历史页、保存/删除确认、导入历史摘要。
- `src/io_util.rs`：复用原子写；如需目录枚举辅助，可放通用函数。
- `src/config.rs`：如果增加“自动保存摘要”开关，默认必须为 false。
- `README.md`：说明历史库位置和隐私边界。

注意事项：

- 删除历史记录需要确认弹窗；AGENTS 明确不要随意删除文件。
- 历史摘要导出默认应是 compact 或 pretty JSON 都可以，但必须脱敏。
- 记录库文件名用时间戳和随机短 ID，不要包含角色名、路径或深渊名称，避免文件名泄露信息。
- 测试覆盖旧版本 JSON 导入、损坏 JSON 跳过、最大记录数裁剪。

### 5. 历史对比与配队比较

目标：基于本地历史记录，比较不同队伍、不同战斗、同一角色在不同场景下的 DPS、技能占比和爆发曲线。

调整方向：

- 在 `src/history.rs` 增加查询和排序辅助，例如按角色、深渊站点、时间范围、队伍成员过滤。
- 在 `src/app.rs` 历史页增加两种视图：列表视图和对比视图。
- 对比视图初版只支持选择 2 条记录，展示总 DPS、角色 DPS、技能占比差异、战斗时间差异。
- UI 使用左右两列或上下两段，不要用复杂嵌套卡片；差异值用颜色和箭头表达。

涉及文件：

- `src/history.rs`：过滤、排序、摘要对比函数。
- `src/model.rs`：必要时新增 `CombatSessionDiff` 纯结构。
- `src/app.rs`：历史选择状态、对比 UI、详情复用组件。

注意事项：

- 不要跨不同 DPS 时间口径直接比较，或必须明确标注“扣除时停/现实时间”。
- 深渊上行线和下行线要分开比较，避免混合场景误导。
- 角色 ID 不存在于当前资源表时仍要显示历史记录中的保存名。

### 6. 深渊预测增强

目标：把当前 `怪物总 HP / 队伍 DPS` 的预测扩展成更实用的深渊规划工具：按波次显示预计耗时，按目标时间反推所需 DPS，从历史记录选择上/下行线队伍，并比较候选队伍。

调整方向：

- 在 `src/abyss_data.rs` 或 `src/model.rs` 增加纯计算函数：`line_hp_by_wave`、`required_dps_for_target_time`、`predict_wave_clear_times`。
- 在 `src/app.rs` 的深渊数值窗口中扩展每条线路 header：显示总 HP、预计耗时、目标时间输入、所需 DPS。
- 增加“从历史选择队伍”入口，读取 `src/history.rs` 的摘要；没有历史模块时可先只增强当前导入队伍。
- 波次展示使用紧凑横条：每波一段，段宽按 HP 占比，tooltip 显示怪物、数量、HP、预计秒数。

涉及文件：

- `src/abyss_data.rs`：如需要按波次汇总怪物 HP，放纯数据计算，不依赖 UI。
- `src/model.rs`：预测结果结构可放这里，保持可测试。
- `src/app.rs`：深渊窗口 UI、目标时间输入、队伍选择状态。
- `src/history.rs`：历史队伍选择完成后接入。
- `README.md`：说明预测只是基于当前 DPS 和静态 HP 的估算。

注意事项：

- 预测不应暗示真实通关必然成立；怪物无敌、转阶段、走位损耗、机制时间都不在静态 HP 模型里。
- 当前 `TeamDpsExport` 只有 DPS 和成员，不能还原技能结构；增强预测不要假设它包含完整战斗。
- UI 文案建议用“预计/估算/所需 DPS”，不要用“必过”。

## 第三阶段：维护效率与稳定性

### 7. 资源覆盖率面板

目标：让维护者快速看到资源表是否完整：角色缺头像/属性、技能缺中文名、GE 未映射、深渊怪物缺图片、反应素材缺失。

调整方向：

- 新增纯检查模块，可以放 `src/resource_audit.rs`，只读取仓库内 `res/`，不触碰授权客户端资源。
- 检查结果结构包括 severity、category、resource id、display name、suggested source。
- 在 `src/app.rs` Debug 或 Console 增加“资源”页，只在 debug 构建或维护入口展示。
- 工具侧 `tools/nte_asset_pipeline.py` 已经会生成报告，主程序面板只做运行资源覆盖检查，不调用 Python。

涉及文件：

- `src/resource_audit.rs`：新增文件，做轻量资源完整性检查。
- `src/app.rs`：新增资源页和筛选 UI。
- `src/parser.rs`：暴露已有资源路径常量时注意不要引入循环依赖。
- `tools/README.md`：如资源面板能辅助维护，补充说明。

注意事项：

- 面板中不要显示客户端导出路径、AES key、usmap 路径。
- 大型 JSON 读取不要在帧内做；首次打开页签时加载，或后台线程加载后 `request_repaint()`。
- 缺失资源不一定影响普通运行，severity 要区分 warning 和 error。

### 8. 自动诊断向导

目标：用户抓不到包或没有伤害时，按步骤检查环境并给出下一步建议，而不是只显示一行错误。

调整方向：

- 复用 `src/network.rs` 的 `HTGame.exe` 检测、Npcap 动态加载错误、设备枚举、BPF 设置结果、原始 PCAPNG 写入状态。
- 新增 `DiagnosticCheck` 结构，建议放 `src/capture.rs` 或新文件 `src/diagnostics.rs`；UI 只消费检查结果。
- 在 `src/app.rs` 的“诊断”页增加“运行诊断”按钮，逐项显示状态：通过、警告、失败、建议。
- 支持复制脱敏诊断报告，不包含本机 IP、网卡 GUID、完整路径或 payload。

涉及文件：

- `src/diagnostics.rs`：可新增，承载诊断模型和组合逻辑。
- `src/network.rs`：必要时拆出更细的检查函数，但保持 Windows API 边界。
- `src/capture.rs`：复用 Npcap 加载/设备枚举错误；不要让诊断影响实时抓包状态。
- `src/app.rs`：诊断向导 UI、复制脱敏结果。

注意事项：

- 诊断不能自动改系统设置、安装 Npcap 或提升权限；只能提示。
- 不要在 egui 帧内阻塞长耗时检查，尤其是网络枚举或文件扫描。
- 失败建议要具体，例如“未检测到 HTGame.exe 活动连接，请先进入游戏场景后再开始抓包”。

### 9. HUD 自定义布局

目标：让用户控制 HUD 中显示哪些模块：总 DPS、战斗时间、角色排行、受击、深渊上下行线、鼠标穿透状态、小型 DPS 曲线。

调整方向：

- 在 `src/config.rs` 增加 HUD 配置结构，例如 `HudConfig`，字段包括模块开关、最大角色数、是否显示标题、曲线开关。
- 在 `src/app.rs` 把当前 `hud_panel()` 拆成小组件：`hud_summary_row`、`hud_character_rows`、`hud_abyss_row`、`hud_mini_timeline`。
- 设置页增加 HUD 区域，使用 checkbox/toggle 和小数字输入，不要塞太多文字说明。
- 保持透明背景和窗口穿透行为不变，避免影响现有游戏内覆盖体验。

涉及文件：

- `src/config.rs`：新增 `HudConfig`，默认保持当前 HUD 行为。
- `src/app.rs`：拆分 HUD 绘制函数，设置页新增 HUD 控制。
- `src/window_attributes.rs`、`src/hotkey.rs`：一般不需要改；只有新增快捷键时再动。

注意事项：

- UI 文案保持中文，按钮和开关要短。
- 小型曲线必须有固定高度，不能因为数据变化导致 HUD 抖动。
- 涉及透明、置顶、穿透、快捷键的改动需要人工验证 Windows 行为。

## 研究分支功能

### 10. 敌方目标识别 / 场景识别研究

目标：继续研究目标 HP 序列、场景上下文和可能的目标槽位，但主线不展示高置信敌方目标识别结果。

调整方向：

- 只在 `research/scene-target-identification` 或用户明确授权的分支中实现实验性 UI。
- 主线可以增加匿名诊断，例如“目标 HP 序列 A/B/C”，但不要显示敌人名称或强归因结果。
- 所有研究输出都必须脱敏，不包含完整载荷、样本 PCAP、本机路径或资源授权信息。

涉及文件：

- 研究分支可新增独立模块；主线最多触碰 `src/model.rs` 的匿名序列摘要和 `src/app.rs` 的 Debug 诊断展示。
- 不要修改 `README.md` 让用户以为主线已经支持敌方目标识别。

注意事项：

- 解析器应宁可“不识别”也不要制造高置信误判。
- 如果某个启发式只对少量样本成立，必须留在 Debug/研究入口。

## 不建议近期做

- 云端排行榜、在线同步、账号体系：会引入隐私、反作弊观感、服务端维护和许可证风险，和本地诊断工具定位冲突。
- 自动安装 Npcap 或自动改系统抓包配置：权限和安全风险高，建议只做诊断提示。
- 大型 UI 框架替换或 egui/eframe 主版本升级：当前 UI 已经深度依赖 egui immediate mode 和多 viewport 行为，升级前必须专项验证。
- 为少量图表引入大型绘图库：短期优先 `egui::Painter`，确有交互需求再评估 `egui_plot`。

## 建议排期

1. `model.rs` 增加时间轴聚合和技能聚合，配套单元测试。
2. `app.rs` 增加 Console 的“时间轴”和“技能”页签，先不新增依赖。
3. 增加回放解析质量报告，形成脱敏问题反馈文本。
4. 新增 `history.rs`，实现手动保存本地战斗摘要。
5. 在历史摘要基础上增强深渊预测和历史对比。
6. 最后做资源覆盖率面板、诊断向导和 HUD 自定义。

这个顺序的好处是：第一批功能直接提升用户分析体验；中间产出的聚合结构可以被历史库和深渊预测复用；维护/诊断工具最后接入，避免过早扩散 UI 状态。
