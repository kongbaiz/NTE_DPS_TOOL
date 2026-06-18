# Packet Content Parser V2

## 2026-06-18 Net Runtime Events

- 新增 `net_event` 层，把之前没有进入状态链的包先归一成事件：`package_map_export`、`iris_reference`、`transport_bunch`、`actor_channel`、`sdk_target_data`、`object_lifecycle`、`ability_lifecycle`、`gameplay_effect_lifecycle`。
- `PackageMap/NetGUID` 和 `Iris` 目前仍基于路径锚点附近的候选值，不声称已经完成 UE `PackageMapClient::SerializeObject` 或 Iris NetSerializer 解码；但事件会统一写入 `Net runtime events` debug note，并把可确认的 `netguid32`、`netguid_packed`、`iris_ref32` 作为 runtime alias 进入 `TargetInstanceStore`。
- `sdk_target_data` 使用 Dumper-7 SDK 推出的 `FCharacterForNet` 0x28 字节 NetTarget 形状，提取 `FClientRepFightData` / `FClientRepExtraDamageInfo` 的 HP 和 dead state 候选。为避免把 BossHP 包误扫成 SDK target-data，实时解码只在 S2C 且存在目标路径、GameplayEffect 或显式 SDK/Ability 文本锚点时启用；不会仅因为 BossHP/CurrentHP 出现就全包扫描。
- 新增 `sdk_net_target` alias，和 `NetRefHandleCandidate:sdk_target:<hex>` 建立等价键；这和 `current_hp_token` 分开，避免把 0x28 字节 SDK NetTarget 与 CurrentHP 的 4 字节弱 token 混用。
- `object_lifecycle` 的 close/destroy 文本事件如果能关联到目标路径，会让同路径 runtime instance 过期并移除其 alias，降低 channel/handle 复用污染。没有路径或别名的生命周期文本只作为 debug 证据保留。
- `transport_bunch` / `actor_channel` 目前记录已识别的 transport/sequenced/single-bunch 轮廓；字段级 ActorChannel id、可靠序列、RPC/property 边界仍未完整解码。
- 在三份真实抓包上验证：`001418` 命中 runtime notes 70、PackageMap 14、Iris 18、SDK target-data 8、transport 52；`030535` 命中 runtime notes 49、PackageMap 3、Iris 3、SDK target-data 0、transport 46；`033550` 命中 runtime notes 73、PackageMap 6、Iris 7、SDK target-data 2、transport 66。原有 hit 数保持 166/48/262，未出现明显倒退。

## 2026-06-18 Runtime Mapping Sidecar

- 新增可选运行时映射 sidecar 输入，导入 `foo.pcapng` 时会自动查找同目录 `foo.sidecar.jsonl`、`foo.sidecar.json`、`foo.runtime.jsonl`、`foo.runtime.json`、`foo.mapping.jsonl`、`foo.mapping.json`，以及 `foo.pcapng.sidecar.jsonl/json`。
- Sidecar 支持 JSON 数组、`{"events":[...]}` 或 JSONL。每条事件至少包含 `time`/`timestamp`/`ts`，并可包含 `event`、`connection`、`channel`、`path`/`object_path`/`class_path`、`target_name`、`netguid32`/`netguid_packed`、`iris_ref32`、`boss_hp_guid`、`current_hp_token`。
- `actor_channel_open`、`netguid_resolved`、`iris_ref_resolved` 等 map 事件会进入 `TargetInstanceStore`，把 ActorChannel/NetGUID/Iris/HP token 作为当前会话实例别名；`close`/`destroy` 类事件会移除或过期相关 alias，避免 channel 复用污染后续命中。
- Sidecar 时间戳如果是小于 `10000000` 的相对秒，并且 pcap 时间戳是 epoch 秒，会自动把第一条 sidecar 事件对齐到第一条 pcap Ethernet 包时间；若 sidecar 已使用 epoch 时间，则原样使用。
- 这层映射是“本次 pcap 会话状态流”，不是全局缓存。静态资源路径和 DataTable 名称可以复用，`actor_channel`、`netguid32`、`iris_ref32` 等数字身份不能跨连接信任。

最小 JSONL 示例：

```jsonl
{"time":1.234,"event":"actor_channel_open","connection":"GameNetDriver.ServerConnection","channel":42,"path":"WorldBoss_Boss33","target_name":"目标名","netguid32":"0x40a54cd0","iris_ref32":"0x2c981ed9"}
{"time":9.876,"event":"actor_channel_close","connection":"GameNetDriver.ServerConnection","channel":42}
```

## 2026-06-18 Net Identity Probe

- 新增 `net_identity` 层，围绕目标路径锚点尝试提取 UE `SerializeIntPacked` 风格 NetGUID 候选、byte-aligned `netguid32` 候选和 `iris_ref32` 候选。
- 提取器只扫描 `Monster/Boss/Enemy/NPC/HTCharacter` 等目标路径前的小窗口，并要求 FString-like 长度前缀或紧邻 packed 整数证据；不会再做全包无锚点扫描。
- 候选进入 `ObjectStateStore` 时仍标记为 `NetGuidCandidate` 或 `NetRefHandleCandidate`，并记录 `path_anchor`、offset、bit shift、raw hex 和 score。没有 HP timeline 或重复证据时，只能作为 possible 级上下文，不会填充 `target_name`。
- `TargetResolver` 对无 HP 的 NetGUID/NetRefHandle 路径锚点按 PathOnly 等级评分；同一路径的 PathOnly 与 Net identity 候选不会互相触发多目标冲突惩罚。
- 在 `nte_raw_20260618_001418_224.pcapng` 上，13 个目标路径锚点保留 29 个候选；`WorldBoss_Boss33` 的 `netguid32:0x40a54cd0`、`iris_ref32:0x2c981ed9`、`netguid_packed:0x2025` 在多个 S2C 路径包中重复出现，可作为后续 NetGUID/Iris 关联的实证线索。

## 2026-06-18 DataTable 名称解析

- `resource_index` 现在会默认加载 `data/DataTable/DT_MonsterManualConfig.json`、`NTE_Assets/DataTable/DT_MonsterManualConfig.json` 以及 `data/DataTable/Monster/`、`NTE_Assets/DataTable/Monster/` 下的 MonsterStatic 表。
- 名称优先级为：`res/data/targets/*.json` 手工覆盖 > MonsterManual `MonsterName` > MonsterStatic `TextName` > MonsterStatic `Comment` 清洗名；同优先级保留先加载项，避免 `data` 中文名被 `NTE_Assets` 英文名覆盖。
- 封包内的实例名、路径名和资源 ID 会生成别名后查表，包括 `mon_023_BP_Trial_C_2147435594` -> `mon_023_BP`、`boss_13_BP_Trial_C_...` -> `boss_13_BP`、`WeeklyClone_Boss26` -> `boss_26_BP`、`MON_015_vision_02` -> `mon_015_BP`、`BP_Boss_07` -> `boss_07_BP` 等。
- 最新多怪抓包中已能从内部 ID/path 解析出 `低语种`、`长明灯`、`罐头锡兵`、`无首铁驭`，并能识别预加载配置中的 `墨菲克斯`、`永不谢幕的阿拉克涅`、`讨债人` 等名称。

## 当前实现目标

本阶段只建立“对象/目标解析基础层”。目标是为后续把每次伤害稳定关联到具体敌人打基础，而不是完整实现 Unreal Engine Generic Replication、PackageMap、NetGUID 或 Iris NetRefHandle 解码。

## 已实现内容

- `ue_bitstream`：LSB-first bit reader、任意 bit offset 读取、shifted byte decode、基础数值读取和 FString-like/path candidate 提取。
- `object_state`：`ObjectStateStore` 保存路径候选、Attribute GUID HP 时间线、对象证据、置信度和过期清理。
- `resource_index`：合并 `res/data/targets/*.json` 手工覆盖、MonsterManual 和 MonsterStatic DataTable，按封包内部 ID/path 生成别名并解析真实怪物显示名；目录或文件不存在时静默降级，已存在文件的读取/JSON/结构错误会产生 warning，并从路径 basename 生成 fallback name。
- `target_resolver`：按可解释 reason 生成 `TargetCandidate`，并只在 probable/confirmed 时填充 `target_name`。
- `PacketDecoder` 集成：S2C 观察 Boss HP、CurrentHP NetTarget 候选和路径候选；C2S 观察伤害、角色声明、GameplayEffect 和路径候选；发送 `Hit` 前附加 `target_id`、`target_name`、`target_context`。
- `runtime_mapping`：可选消费外部 sidecar 中的 ActorChannel/PackageMap/Iris 运行时映射事件，将 NetGUID/Iris/HP token 作为 `TargetInstanceStore` 的强 alias，并在事件到达时回填最近 Hit。
- AttributeGuid 只会在短时间窗口内存在唯一高置信近邻目标路径时链接路径，从而让 HP 属性实例获得 `object_path` / fallback name。
- CurrentHP 的 16 字节前缀只提取未被固定模式约束的 4 个变动字节，作为低置信 `NetRefHandleCandidate` token 进入 `ObjectStateStore`。它不会单独确认目标；只有同一 token 的 HP 时间线与伤害 delta 或 `target_hp_before/after` 对齐时，才会参与目标回填。
- 基于 Dumper-7 SDK 中 `FCharacterForNet`、`FClientRepExtraDamageInfo` 和 `FClientRepFightData` 的字段大小，保留了离线候选扫描函数；但完整进图包显示泛化扫描噪声过高，实时 `PacketDecoder` 默认不启用这类无锚点扫描。
- Boss/Monster 路径与 HP AttributeGuid 的链接窗口为 6 秒；一旦 AttributeGuid 已链接到某个目标路径，后续召唤物、技能或掉落路径不会覆盖它，只记录 `conflicting_path_link` 证据。
- 多个强 targetish 路径同时出现时，只记录 `ambiguous_path_link:<count>` 证据，不写入 `object_path` / `display_name`，避免把多 Boss、召唤物、分身或预加载路径误认为命中目标。
- 原始 Hit 仍立即发送；后到的 S2C HP/path 证据会通过 `HitTargetUpdate` 回填最近 Hit 的 target 字段，但只允许 unknown -> possible/probable/confirmed、possible -> probable/confirmed、probable -> confirmed、同目标更高 score 或直接 HP 证据支持的同级/升级结果，不做降级覆盖。
- 覆纹推断 Hit 也会经过同一套 TargetResolver。

## 未实现内容

- 未实现完整 ActorChannel/Bunch 字段级解析。
- 未实现 PackageMap/SerializeObject 的 NetGUID 建图。
- 未实现 Iris NetRefHandle 和 NetSerializer 解码。
- 未解析 PacketHandler 加密、认证、完整性校验，也不会尝试绕过。
- 未建立 StringTable/locres 的完整文本解析；当前怪物名称主要来自 MonsterManual/MonsterStatic DataTable 和手工覆盖资源。
- CurrentHP token 仍是候选身份，不等价于已完整解出的 Actor、Monster 或 Iris `FNetRefHandle`；缺少 HP 时间线匹配时不会提升到 probable/confirmed。

## 已知限制

- 当前对象路径、类路径和目标候选均为 heuristic/candidate。
- Attribute GUID 目前只确认可作为 HP 属性实例候选，不等价于 Actor 或 Monster 实例。
- HitTargetUpdate 只回填短时间窗口内的最近 Hit；跨长窗口或乱序严重的包仍可能无法更新。
- 多目标、多 Boss、召唤物、分身场景如果缺少可区分 HP/handle 证据，仍可能只能输出 possible/unknown；此时即使路径能解析出名称，也不会把它伪装成已确认的本次命中目标。
- 当前仍未实现 ActorChannel、PackageMap、NetGUID 或 Iris NetRefHandle 的完整建图；路径、AttributeGuid 与 HP timeline 的关联仍是第一阶段 heuristic/candidate 目标解析。

## 目标匹配评分规则

- AttributeGuid 已链接近邻 Monster/Boss/Enemy/Character/HTCharacter 等路径：`+30`。
- AttributeGuid 已链接 `/Game/` 且包含 Monster/Boss/NPC/Enemy 的路径：额外 `+40`。
- 只有 PathOnly、没有 HP/handle 证据的路径候选：最高 possible，不填 `target_name`。
- Boss HP update 的 HP delta 与伤害在 1 秒窗口内匹配：`+50`。
- `target_hp_before`/`target_hp_after` 与同一 HP GUID 时间线匹配：`+50`。
- CurrentHP `NetRefHandleCandidate` 的 HP delta 或 HP before/after 时间线匹配：`+50`，reason 使用 `net_target_hp_delta_match` 或 `net_target_hp_timeline_match`。
- 时间差越小额外加分，最高 `+20`。
- 当前窗口只有一个高置信 Boss/Monster 对象：`+35`（有 HP 证据）或 `+25`。
- 只有 `target_max_hp` 大小判断：最多 `+5`。
- 多候选无直接 HP 证据时：`-20`；直接 HP 证据包括 `hp_guid_timeline_match`、`boss_hp_delta_match`、AttributeGuid 的 `last_hp_close_to_hit_after`。

置信度：

- `score >= 90`：confirmed
- `60 <= score < 90`：probable
- `35 <= score < 60`：possible
- `score < 35`：unknown

## 为什么低置信不填 target_name

`target_name` 会被 UI 和导出 JSON 视为对用户有直接解释力的字段。possible/unknown 只代表候选证据存在，不能证明本次攻击命中了该对象。低置信时只写入 `target_context`，保留 score、reason、path、GUID 等证据，避免把启发式结果伪装成确定敌人名称。

## 参考资料

本阶段联网查阅过以下资料，均只作为结构设计参考；NTE 实际协议格式仍以本仓库抓包证据为准。

- Epic UE API: UActorChannel - https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/Engine/UActorChannel
- Epic UE API: UChannel - https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/Engine/Engine/UChannel
- Epic UE API: FNetBitReader - https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/CoreUObject/FNetBitReader
- Epic UE Iris overview - https://dev.epicgames.com/documentation/unreal-engine/iris-replication-system-in-unreal-engine
- Epic UE Iris components / FNetRefHandle - https://dev.epicgames.com/documentation/unreal-engine/components-of-iris-in-unreal-engine
- Epic UE Iris filtering / FNetRefHandle usage - https://dev.epicgames.com/documentation/unreal-engine/iris-filtering-in-unreal-engine
- Epic UE FastArraySerializerHeader - https://dev.epicgames.com/documentation/en-us/unreal-engine/API/Runtime/NetCore/FFastArraySerializer/FFastArraySerializerHeader
- Epic UE PacketHandler - https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/PacketHandler/PacketHandler
- Epic UE HandlerComponent - https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/PacketHandler/HandlerComponent
- Epic UE EncryptionComponent - https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/PacketHandler/FEncryptionComponent
- Epic UE Oodle Network - https://dev.epicgames.com/documentation/unreal-engine/oodle-network
- GitHub boxcars - https://github.com/nickbabcock/boxcars
- GitHub rrrocket - https://github.com/nickbabcock/rrrocket
- GitHub subtr-actor - https://github.com/rlrml/subtr-actor
- GitHub CUE4Parse - https://github.com/FabianFG/CUE4Parse
- GitHub UEExtractor - https://github.com/SolicenTEAM/UEExtractor

## 后续阶段计划

1. Generic ActorChannel / PackageMap / NetGUID 解析。
2. Iris NetRefHandle 解析。
3. StringTable/locres 补全和更多场景 DataTable 覆盖。
4. 多目标最小费用匹配。
5. Networking Insights / NetTrace 对照验证。
