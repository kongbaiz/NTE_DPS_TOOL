# NTE 实时 DPS 工具

Rust + egui 实现的 NTE 队伍实时 DPS 统计工具。伤害协议解析逻辑由
`nte_packet_parser.py` 迁移而来，直接通过 Npcap 捕获本机发出的 UDP 数据。

## 功能

- 实时统计总伤害、DPS、命中数和战斗时间
- 按角色显示伤害、占比、命中数和 DPS
- 实时命中明细与累计伤害曲线
- 独立 Debug 面板，显示封包端点、角色声明、解析结果和载荷预览
- 实时流式保存完整 Ethernet 帧为 `logs/nte_raw_*.pcapng`
- 支持另存完整 PCAPNG，以及单独导出筛选后的解析 JSON
- Debug 窗口按封包、角色数据、环境分栏，并可编辑或新增 `characters.json` 记录
- 支持回放 `logs/nte_hits_*.jsonl`，无需进入游戏即可验证界面
- 动态加载 Npcap，不需要安装 Npcap SDK
- 根据 `HTGame.exe` 的活动连接自动选择网卡和本机 IP

## 环境

- Windows 10/11
- Rust 1.85 或更高版本
- [Npcap](https://npcap.com/)，建议启用 WinPcap API-compatible Mode
- 实时抓包可能需要以管理员身份运行

## 运行

```powershell
cargo run --release
```

程序会自动查找 `HTGame.exe`，优先使用远端端口 `30031` 的活动 TCP 连接定位
本机 IP，再匹配对应的 Npcap 网卡。点击“开始抓包”时会重新检测，无需手动选择
网卡。默认 BPF 过滤器为 `udp`。

开始实时抓包后，程序会把所有通过当前 BPF 过滤器的原始帧直接写入
`logs/nte_raw_*.pcapng`。该路径不经过 Debug 包筛选，也不受界面最多保留
10,000 个解析包的限制；PCAPNG 包含链路层头、原始时间戳、捕获长度和线上长度。
原始文件写入失败时，现有伤害和场景解析仍会继续运行。

Debug 面板支持导入完整 PCAPNG 或解析 JSON，并使用与实时抓包相同的解析流程。

## 验证

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

测试包含 Python 合成载荷兼容性检查，以及仓库真实抓包日志的解析结果交叉检查。
