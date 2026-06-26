# To Do List

## 大招时停时间表

- [ ] 从 `NTE_Assets/DataTable/Skill/DT_MediaFFmpegInfo.json` 提取普通角色大招媒体条目，记录 `media_id` 和 `Path.FilePath`。
- [ ] 用现有资源 probe 结合合法客户端资源、`usmap` 和授权解密材料导出对应 `Content/Movies/FFMpeg/Skill/*.mp4`；不要把 AES key、完整本机路径或导出目录写入日志和报告。
- [ ] 用 `ffprobe` 或等价工具读取视频 duration，生成 `character_id / ability_id / media_id / duration_seconds / source / confidence` 表。
- [ ] 浔单独处理：优先使用抓包里的 `Event.Montage.Player.UltraSkill.Jin.EnterTimeStop` 和 `Event.Montage.Player.UltraSkill.Jin.ClearTimeStop`，未闭合区间裁剪到当前半场或战斗结束；资产中的 `Jin_MaxTimeValue / Jin_TimeValueConsumePerSecond = 12s` 只作为兜底。
- [ ] 对没有 `DT_MediaFFmpegInfo` 媒体条目的角色，继续查 GA 蓝图、Level Sequence 或 Montage 引用；不要硬套其他角色的视频时长。
- [ ] 用真实抓包抽样校准：普通角色以 `CoolDown.Player.UltraSkill.*` 作为 Q 起始候选，检查视频时长结束点是否接近角色恢复可操作后的后续输入或动作封包。
- [ ] 表生成后再接入 DPS 计算，作为“游戏时间”模式扣除普通大招动画时停的依据。
