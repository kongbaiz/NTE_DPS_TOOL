# NTE 实时 DPS 工具

Rust + egui 实现的 NTE 队伍实时 DPS 统计工具。直接通过 Npcap 捕获本机发出的 UDP 数据。

## 功能

- 实时统计总伤害、DPS、命中数和战斗时间
- 按角色显示伤害、占比、命中数和 DPS
- 实时命中明细与累计伤害曲线
- 独立 Debug 面板，显示封包端点、角色声明、解析结果和载荷预览
- 实时流式保存完整 Ethernet 帧为 `logs/nte_raw_*.pcapng`
- 支持另存完整 PCAPNG，以及单独导出筛选后的解析 JSON
- 支持将 `.pcapng` 直接拖入主窗口导入；管理员权限运行时可使用主界面的
  “导入 PCAPNG”按钮或 `Ctrl+O`（Windows 禁止普通权限资源管理器向管理员窗口拖放）
- 自动保存透明度、明暗主题和窗口置顶设置到
  `%LOCALAPPDATA%\NTE DPS Tool\config.json`
- Debug 窗口按封包、角色数据、环境分栏，并可编辑或新增
  `res/data/characters/characters.json` 记录
- 动态加载 Npcap，不需要安装 Npcap SDK
- 根据 `HTGame.exe` 的活动连接自动选择网卡和本机 IP

## 环境

- Windows 10/11
- Rust 1.85 或更高版本
- [Npcap](https://npcap.com/)，建议启用 WinPcap API-compatible Mode
- 实时抓包可能需要以管理员身份运行

## Clone 后运行

```powershell
git clone https://github.com/kongbaiz/NTE_DPS_TOOL.git
cd NTE_DPS_TOOL
cargo test
cargo run --release
```

运行程序所需的 `res` 数据、角色图片和应用图标均已纳入仓库。普通使用不需要
CUE4Parse、FModel、Python 或 Npcap SDK；只需安装 Rust 与 Npcap。

程序会自动查找 `HTGame.exe`，优先使用远端端口 `30031` 的活动 TCP 连接定位
本机 IP，再匹配对应的 Npcap 网卡。点击“开始抓包”时会重新检测，无需手动选择
网卡。默认 BPF 过滤器为 `udp`。

开始实时抓包后，程序会把所有通过当前 BPF 过滤器的原始帧直接写入
`logs/nte_raw_*.pcapng`。该路径不经过 Debug 包筛选，也不受界面最多保留
10,000 个解析包的限制；PCAPNG 包含链路层头、原始时间戳、捕获长度和线上长度。
原始文件写入失败时，现有伤害和场景解析仍会继续运行。

Debug 面板支持导入完整 PCAPNG 或解析 JSON，并使用与实时抓包相同的解析流程。

## 资源目录

程序运行时资源统一放在 `res`：

```text
res/
  data/characters/   角色配置
  data/skills/       技能与伤害映射表
  images/characters/ 角色头像
  images/attributes/ 属性图标
  icons/             应用图标
```

程序会从当前目录或可执行文件的上级目录查找 `res`。角色及属性图片也会在
编译时内嵌，作为外部图片缺失时的降级资源。

## 资源更新

维护资源时额外需要：

- Git
- PowerShell 5.1 或更高版本
- Python 3.10 或更高版本

首次在新设备维护资源时执行：

```powershell
powershell -ExecutionPolicy Bypass -File tools/bootstrap-tools.ps1
python -m pip install -r tools/requirements.txt
```

Bootstrap 会按照 `tools/external-tools.json` 安装固定版本的 .NET SDK、拉取固定
提交的 CUE4Parse，并构建项目专用探测器。所有下载内容和构建结果都位于被 Git
忽略的 `tools/external`、`tools/cue4parse_probe/bin` 和
`tools/cue4parse_probe/obj`。

`tools/nte_asset_pipeline.py` 用于盘点 UE 容器，并从已有的导出目录生成程序资源。

先查看客户端容器及必须导出的资源：

```powershell
python tools/nte_asset_pipeline.py inventory `
  --paks-dir "D:\Neverness To Everness\Client\WindowsNoEditor\HT\Content\Paks" `
  --output target/nte_container_inventory.json
```

导出完成后，先生成到预览目录并检查覆盖率报告：

```powershell
python tools/nte_asset_pipeline.py build `
  --assets-root NTE_Assets `
  --output-res target/resource-preview/res `
  --existing-res res
```

确认 `target/resource-preview/res/data/asset_report.json` 后，将 `--output-res`
改为 `res` 即可更新正式资源。脚本会生成角色、技能、伤害映射、异能环合、
技能说明、头像、属性图标和输入文件哈希清单；默认保留现有角色的颜色及数据表
之外的人工记录。

### 客户端容器探测

第三方工具安装在被 Git 忽略的 `tools/external`，版本与校验值记录在
`tools/external-tools.json`。项目内的 CUE4Parse 探测器使用官方源码中专门为
NTE 提供的 `GAME_NevernessToEverness` 配置：

```powershell
$dotnet = "tools/external/dotnet10/dotnet.exe"

& $dotnet tools/cue4parse_probe/bin/Release/net10.0/Cue4ParseProbe.dll `
  --paks "D:\Neverness To Everness\Client\WindowsNoEditor\HT\Content\Paks" `
  --output target/cue4parse-export
```

客户端容器索引已确认使用 AES 加密。若拥有合法授权的 32 字节 AES key，可将
其单独保存为不纳入版本控制的文本文件，并增加：

```text
--aes-key-file <key-file>
```

如果加载 DataTable 时提示缺少类型映射，再增加 `--usmap <mapping-file>`。
探测器不会记录或输出 AES key。

### 直接生成 res 数据

`tools/export_nte_res.py` 会调用 CUE4Parse，从客户端容器导出指定 DataTable，
再转换为程序使用的 `res` 目录结构。AES key 不应写入仓库，可放在独立文本文件中：

```powershell
python tools/export_nte_res.py `
  --paks-dir "D:\Neverness To Everness\Client\WindowsNoEditor\HT\Content\Paks" `
  --usmap NTE-5.6.1.usmap `
  --aes-key-file "D:\private\nte-aes-key.txt" `
  --output-res target/direct-res-preview/res
```

不指定 `--table` 时会导出全部项目数据。也可以重复指定需要更新的数据组：

```powershell
python tools/export_nte_res.py `
  --paks-dir "D:\Neverness To Everness\Client\WindowsNoEditor\HT\Content\Paks" `
  --aes-key-file "D:\private\nte-aes-key.txt" `
  --table gameplay-effect-mapping `
  --table skill-damage `
  --table wooden-descriptions `
  --output-res res
```

可选数据组为 `gameplay-effect-mapping`、`skill-damage`、
`wooden-descriptions`、`characters`、`ability-tips`、`reactions` 和 `all`。
原始 CUE4Parse JSON 默认保存在 `target/nte-direct-export`，转换报告写入
`res/data/direct_export_report.json`。

### 其他维护脚本

- `tools/unpack_nte_reslist.py`：解密并解压启动器的 ResList/lastdiff 清单
- `tools/analyze_nte_ini.py`：分析 NTE 加密 INI，报告会脱敏敏感字段
- `tools/nte_asset_pipeline.py`：从现成导出树生成资源和覆盖率报告

这些脚本生成的 `target`、`logs`、`NTE_Assets`、C# `bin/obj` 和第三方工具目录
均不会进入版本控制。

## 算法文档

封包字段、Boss HP、覆纹逐段推算、DPS 与统计公式详见
[NTE_封包解析算法.md](NTE_封包解析算法.md)。

## 验证

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

依赖真实抓包的诊断测试默认忽略。需要运行时设置
`NTE_TEST_CAPTURE=<pcapng-path>`，再执行：

```powershell
cargo test -- --ignored
```

### Scene index

The application does not load the full UE world export at runtime. After
updating the unpacked resources, regenerate the compact scene index with:

```powershell
python tools/build_scene_index.py
python tools/build_monster_index.py
```

The generated `res/data/scenes/scene_index.json` is embedded into the
executable and maps DataLayer GUIDs and selected World Partition cell IDs to
displayable scene names.

Use `--include-all --output res/data/scenes/scene_index_all.json` to regenerate
the review-only full scene list. `res/data/monsters/monster_index.json` maps
runtime monster classes and GameplayEffect source families to Chinese names.
