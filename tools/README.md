# NTE DPS Tool 资源工具

本目录保存资源维护脚本和 CUE4Parse probe。普通运行程序不需要执行这些脚本；只有更新 `res/` 数据或排查客户端资源时才需要。

## Python 环境

工具脚本要求 Python 3.14 或更高版本，依赖见 `pyproject.toml` / `requirements.txt`。可以使用已有的 `.venv`，也可以按本机习惯创建虚拟环境。

```powershell
python -m pip install -r tools/requirements.txt
```

## 直接从客户端导出 res

`export_nte_res.py` 会调用项目内的 CUE4Parse probe，从客户端容器导出项目需要的数据表，并转换到 `res/`。

```powershell
$env:NTE_AES_KEY = "<64位十六进制AES key>"
python tools/export_nte_res.py `
  --paks-dir "D:\Neverness To Everness\Client\WindowsNoEditor\HT\Content" `
  --usmap ".\NTE-5.6.1.usmap" `
  --table all
```

可选 `--table` 值：

- `gameplay-effect-mapping`
- `skill-damage`
- `wooden-descriptions`
- `characters`
- `ability-tips`
- `reactions`
- `all`

默认输出到仓库内 `res/`，原始导出和报告写入 `target/nte-direct-export`。如果不想使用环境变量，也可以用 `--aes-key-file <path>` 指定只包含资源导出 AES key 的文件。

## 从已解包资源生成 res

如果已经有 FModel、CUE4Parse 或其他工具导出的 `NTE_Assets` 树，可以只运行后处理：

```powershell
python tools/nte_asset_pipeline.py build --assets-root .\NTE_Assets --output-res .\res
```

该流程会生成或更新：

- `res/data/characters/characters.json`
- `res/data/skills/gameplay_effect_mapping.json`
- `res/data/skills/skill_damage.json`
- `res/data/skills/wooden_damage_descriptions.json`
- `res/data/skills/ability_tips.json`
- `res/data/reactions/reactions.json`
- `res/data/abyss/season_names_zh_cn.json`
- `res/data/asset_report.json`
- `res/data/asset_manifest.json`

默认会保留现有角色配置里的人工颜色和旧角色记录；需要删除角色表中不存在的旧记录时再加 `--prune-stale-characters`。

## 辅助分析

盘点客户端容器：

```powershell
python tools/nte_asset_pipeline.py inventory --paks-dir "<HT Content路径>" --output target/paks-inventory.json
```

解密启动器 ResList/lastdiff 清单：

```powershell
python tools/unpack_nte_reslist.py "<清单文件或目录>" --output-dir target/reslist-analysis
```

分析 NTE 加密 INI：

```powershell
python tools/analyze_nte_ini.py "<ini文件或目录>" --output target/ini-analysis
```

## 提交规则

可以提交手工维护的脚本和稳定 `res/` 资源。不要提交 `target/`、`logs/`、`NTE_Assets/`、第三方工具目录、C# `bin/obj`、资源导出 AES key、usmap 或完整解包数据。主程序内置的加密 INI 协议 key 不用于资源导出。
