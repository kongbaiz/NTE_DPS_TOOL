# NTE 实时 DPS 工具

Rust + egui 实现的 NTE 队伍实时 DPS 统计工具。伤害协议解析逻辑由
`nte_packet_parser.py` 迁移而来，直接通过 Npcap 捕获本机发出的 UDP 数据。

## 功能

- 实时统计总伤害、DPS、命中数和战斗时间
- 按角色显示伤害、占比、命中数和 DPS
- 实时命中明细与累计伤害曲线
- 独立 Debug 面板，显示封包端点、角色声明、解析结果和载荷预览
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

点击“回放 JSONL”可选择命中日志；“回放最新日志”会读取 `logs` 目录中最新的
`nte_hits_*.jsonl`。

## 验证

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

测试包含 Python 合成载荷兼容性检查，以及仓库真实抓包日志的解析结果交叉检查。
