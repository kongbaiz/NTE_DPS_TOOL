# NTE DPS TOOL

Rust + egui 实现的 NTE 实时 DPS 工具。工具通过 Npcap 抓取本机 UDP 流量，解析伤害、深渊事件和部分 GameplayEffect 信息，并在本地展示总览、角色、技能、命中明细和深渊上下行线统计。

## 功能

- 实时统计总伤害、DPS、命中数、受击统计和战斗时长。
- 按角色展示伤害、占比、命中数、DPS、受击统计、技能分类和可筛选命中明细。
- 保留目标 HP 数值字段：`target_hp_before`、`target_hp_after`、`target_max_hp`、`target_hp_percent`。
- 解析并展示 GameplayEffect 映射、技能分类、`ability_name`、`damage_name`、`attack_type`。
- 深渊上行线/下行线独立统计，保留重开、进入线路、通关和离开事件状态，并提供深渊怪物数值表查看。
- 将 `GA_CardTrigger_*` / `GE_AbyssCard_*_Damage` 这类异境补给站可选场地 Buff 伤害归类为 `深渊场地Buff`，避免混入角色技能或创生花。
- 实时保存完整 Ethernet 帧到 `logs/nte_raw_*.pcapng`。
- 支持导出解析后的 JSON、另存完整 PCAPNG，支持导入 JSON 和 PCAPNG 进行 Debug 回放。
- Debug 面板可查看封包端点、角色声明、解析结果和载荷预览。
- Debug 面板支持编辑或新增 `res/data/characters/characters.json` 角色数据，支持打开、搜索、编辑并保存 NTE 加密 INI。
- Debug 面板提供环境诊断、网卡列表、原始抓包路径、服务端伤害校准开关和导入导出入口。
- 自动保存透明度、深浅色主题、窗口置顶和服务端伤害校准设置到 `%LOCALAPPDATA%\NTE DPS Tool\config.json`。
- 支持 Home 快捷键切换鼠标穿透；Debug 构建支持 F12 打开/关闭 Debug 面板。
- 根据 `HTGame.exe` 的活动连接自动选择网卡和本机 IP。

具体敌方目标识别与场景识别仍在研究中，代码保留在 `research/scene-target-identification` 分支。稳定主线不再主动填充或显示 `target_id`、`target_name`、`target_context`，这些字段仅作为旧 JSON 兼容字段保留。

## 环境

- Windows 10/11
- Rust 1.85 或更高版本
- [Npcap](https://npcap.com/)，建议启用 WinPcap API-compatible Mode
- 实时抓包可能需要以管理员身份运行

## 运行

```powershell
git clone https://github.com/kongbaiz/NTE_DPS_TOOL.git
cd NTE_DPS_TOOL
cargo test
cargo run --release
```

普通使用只需要 Rust、Npcap 和仓库内的 `res` 资源。不需要 `data/DataTable`、CUE4Parse、FModel、Python、Npcap SDK、资源导出 AES key 或 usmap。Debug 面板的加密 INI 编辑器使用代码内置的稳定 INI 协议 key，不需要用户提供导出密钥。

开始实时抓包后，程序会把通过当前 BPF 过滤器的原始帧写入 `logs/nte_raw_*.pcapng`。Debug 面板可导入完整 PCAPNG 或解析 JSON，并使用与实时抓包相同的稳定解析流程；停止抓包后可另存当前完整 PCAPNG。

## 资源目录

```text
res/
  data/characters/   角色配置
  data/skills/       GameplayEffect、技能、伤害名称和分类映射
  data/reactions/    环合反应和反应图片配置
  data/abyss/        深渊怪物静态表、数值表和字段中文名
  images/characters/ 角色头像
  images/attributes/ 属性图标
  images/font/       游戏伤害数字字体素材
  images/monsters/   深渊怪物头像
  images/reactions/  环合反应文字素材
  icons/             应用图标
```

程序会从当前目录或可执行文件上级目录查找 `res`。角色、属性、伤害数字、反应文字和深渊怪物图片会在编译时内嵌，作为外部图片缺失时的降级资源。

## 资源维护

资源维护脚本位于 `tools/`：

- `tools/nte_asset_pipeline.py`：从已有导出树生成稳定资源和覆盖率报告。
- `tools/export_nte_res.py`：直接调用项目内 CUE4Parse probe 导出稳定 DataTable。
- `tools/unpack_nte_reslist.py`：解密并解压启动器 ResList/lastdiff 清单。
- `tools/analyze_nte_ini.py`：分析 NTE 加密 INI，报告会脱敏敏感字段。

可导出的稳定数据组包括 `gameplay-effect-mapping`、`skill-damage`、`wooden-descriptions`、`characters`、`ability-tips`、`reactions` 和 `all`。深渊怪物数据当前作为稳定运行资源放在 `res/data/abyss/`，主程序直接读取。

更多命令见 `tools/README.md`。脚本生成的 `target`、`logs`、`NTE_Assets`、C# `bin/obj`、第三方工具目录、资源导出 AES key、usmap 和解包数据不应提交。

## 验证

```powershell
cargo fmt --check
cargo check
cargo test
```

依赖真实抓包的诊断测试默认忽略。需要运行时设置 `NTE_TEST_CAPTURE=<pcapng-path>`，再执行：

```powershell
cargo test -- --ignored
```
