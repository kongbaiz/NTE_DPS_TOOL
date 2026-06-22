# NTE 伤害封包解析算法

## UDP 传输层

2026-06-12 的完整登录、传送、大世界 Boss 和深渊抓包包含 20,832 个游戏主会话
UDP 包。除最初 4 个无状态握手包外，20,828 个包都符合固定 72-bit 头：

| bit | 长度 | 解释 |
|---:|---:|---|
| 0 | 3 | PacketHandler 前缀 |
| 3 | 3 | 固定签名 `3`；握手包为 `7` |
| 6 | 2 | 传输模式，已观察到 `0..3` |
| 8 | 2 | 头部标志 |
| 10 | 14 | 对端已确认 PacketId |
| 24 | 14 | 本包 PacketId，模 16384 递增 |
| 38 | 32 | ACK 历史位图 |
| 70 | 2 | 包标志 |
| 72 | 可变 | 应用层位流 |

包尾使用 Unreal 风格终止位：最后一个字节的最高置位是终止标记，不属于载荷。
因此应用层有效 bit 长度必须先去掉终止位，再减去 72。

`mode=0` 是绝大多数普通游戏包。`mode=1` 在登录和传送时出现成组的
900/215 字节块，`mode=2/3` 也在连接建立或批量同步附近出现；其结构已能分离，
但具体业务名称尚无充分证据。

### 单块 Bunch

`mode=0` 中有 474 个包满足以下严格边界，可独立提取并按 14-bit sequence 重组：

| 应用载荷 bit | 长度 | 解释 |
|---:|---:|---|
| 0 | 13 | 前缀/通道组合值 |
| 13 | 14 | Bunch sequence |
| 27 | 8 | 描述值；473 个样本为 `0xcc` |
| 35 | 13 | 数据 bit 长度 |
| 48 | 可变 | Bunch 数据 |
| 48 + 长度 | 1 | 固定尾标志 `1` |

完整登录同步中的连续 968 字节块使用递增 sequence，声明数据长度为 7616 bit。
当前实现将它命名为 `SingleBunch`；前缀各 bit 的最终 Unreal 业务名称仍需结合
客户端函数或更多对照样本确认。

### UDP 方向推断

若已检测到本机游戏 IP：

```text
src == local_ip  => C2S
src != local_ip  => S2C
```

无法获得本机 IP 时使用以下回退：

```text
私网 -> 公网 => C2S
公网 -> 私网 => S2C
```

若两端同为私网或同为公网：

```text
单角色声明包 => C2S
已记录的客户端端点 => C2S
其他 => S2C
```

当 C2S 包包含角色声明时，会把 `(源IP, 源端口)` 记为客户端端点，供后续没有
角色声明的包回退判断。

### Debug 包保留

满足任一条件时保留到 Debug 列表：

```text
解析伤害数 > 0
存在角色声明
存在可读协议文本
存在 CurrentHP/Boss HP 更新
```

否则仅保留非填充且长度大于 96 字节的二进制载荷。全相同字节构成的载荷视为
填充包。

二进制诊断使用零字节比例和 Shannon 熵：

```text
p(x) = 字节x出现次数 / 载荷长度
entropy = -Σ p(x) * log2(p(x))
```

无文本 S2C 包在长度至少 300、零字节占比至少 20%、熵不超过 5.5 bit/byte 时，
标记为可能的 Unreal 位打包复制增量。

## 结论

Rust 旧实现把记录描述为“裸 F0 + F1-F9”。真实抓包证明 F0 也带有字段头，
完整记录由 10 个连续字段组成。每个字段使用相同的位流编码：

```text
[type:u8][length:u32 little-endian][payload:length bytes]
```

字段可以从 UDP 载荷中的任意 bit alignment 开始。字节按 LSB-first 方式从相邻
源字节拼接：

```text
value = data[offset] >> shift
      | data[offset + 1] << (8 - shift)
```

## 完整记录

| 字段 | type | length | 解释 |
|---|---:|---:|---|
| F0 | 12 | 4 | 本次伤害，float32 |
| F1 | 12 | 4 | 受击前当前 HP，float32 |
| F2 | 12 | 4 | 当时最大 HP，float32 |
| F3 | 13 | 8 | 高精度游戏时间，float64 |
| F4 | 12 | 4 | 世界时间，float32 |
| F5 | 12 | 4 | 伤害副本，float32，必须与 F0 相等 |
| F6 | 6 | 4 | 状态位 0，int32 |
| F7 | 6 | 4 | 状态位 1，int32 |
| F8 | 6 | 4 | 状态位 2，int32 |
| F9 | 12 | 4 | 尾部浮点值 |

整条记录解码后固定为 94 字节。F6/F7=`1/0` 仅出现于部分 F1=`1` 的终结记录，
是终结态候选。F9 在已分析记录中均为 0。F6-F9 的最终业务名称尚未确认。

F8 **不是伤害方向**。2026-06-13 的对照抓包中，9 条 F8=`0` 记录仍然连续降低
Boss 的 `1,940,137` HP；真正的玩家受击记录是 `3,101` 伤害，
HP `22,397.898 -> 19,296.898`，且 F8=`1`。

当前实现只从判定为 C2S 的包中解析伤害记录；S2C 包用于 CurrentHP/Boss HP
同步、深渊事件和 Debug 诊断。伤害方向按角色声明和状态位判断：

- 若同 bit alignment 的角色声明出现在伤害记录之后，表示该角色是受击目标，记为 `incoming`。
- 若状态位为 `[0, 1, 0]`，且已解析到角色 ID，也记为 `incoming`。
- 若同包或同会话可解析出角色 ID，且不满足上述受击条件，记为 `outgoing`。
- 若只能使用会话回退角色且多角色声明无法确认，会先记为 `unknown` 候选。

多角色包中的 `unknown` 候选会暂存 `0.5` 秒。如果同时间窗内出现
`packet` / `gameplay_effect` 确认的同一伤害事件，则丢弃候选；如果
GameplayEffect 名称能匹配角色英文名，则可把会话回退命中提升为 `outgoing`。
Debug 选项关闭 incoming 时，`incoming` 记录只写入封包备注，不进入统计。

## 识别流程

1. 对 UDP 应用层载荷的每个字节位置尝试 `bit_shift=0..7`。
2. 从候选位置连续读取 10 个字段。
3. 严格检查 type 序列为
   `12,12,12,13,12,12,6,6,6,12`。
4. 严格检查 length 序列为 `4,4,4,8,4,4,4,4,4,4`。
5. 检查 F0、F1、F2、F3、F4、F5、F9 为有限数值，且 F3/F4 非负。
6. 检查 `2 <= F0 <= 1e9`、F1 非负、F2 大于 0。
7. 检查 F0 与 F5 在相对误差 `1e-6`、绝对误差 `0.01` 内相等。
8. 接受记录并保留 F3、F4、F6-F9；DPS 使用 F0。

因为完整字段头和固定长度序列已经提供足够强的边界，解析器不再依赖
`200..700` 的第三方工具扫描窗口，而是扫描整个 UDP 载荷。

## GameplayEffect / 技能归因

IDA 中可确认 `FHTClientActiveGE`、`UniqueIndex`、`GameplayEffectMappingDataTable`
等运行时字段含义。结合 2026-06-13 的抓包，Active GameplayEffect 在任意
bit alignment 下可按以下边界读取：

```text
"FHTClientActiveGE"
+5 bytes
[marker:u32 = 12]
[UniqueIndex:u32]
```

`UniqueIndex` 可通过
`res/data/skills/gameplay_effect_mapping.json` 映射到
GameplayEffect 名称，再通过 `res/data/skills/skill_damage.json`
关联技能和伤害类型。

已验证样本包括：

| UniqueIndex | GameplayEffect | 对应封包 |
|---:|---|---|
| 1012 | `GE_Player_Sagiri_QTE1_Damage` | 角色 1003，伤害 8772 |
| 241 | `GE_Player_Nanally_Melee1_Damage` | 角色 1010，伤害 3177 |

对现有 5 个抓包扫描得到 1517 条 Active GameplayEffect 记录、98 个不同效果，
并可稳定对应 Jin、Haniel、Lacrimosa、Nanally、Sagiri 等角色的伤害记录。
解析结果当前写入封包调试备注，用于继续确认同包多效果时的归因顺序。

2026-06-14 新抓包进一步得到 678 条有效记录、148 个不同效果，并确认反应效果：

| UniqueIndex | GameplayEffect | 反应 | 实测 |
|---:|---|---|---|
| 52 | `GE_ActorReaction_1_Damage` | 创生相关伤害 | 36 次，共 185066 |
| 559 | `Buff_Reaction_4_new` | 黯星 | 1 次，31500 |
| 561 | `Buff_Reaction_5_new` | 浊燃 | 12 次，共 28673 |

浊燃的 12 次记录均与伤害字段同包，伤害为 `1488`、`2654` 或 `3185`；连续结算段
内间隔约 1 秒。因此浊燃可以直接按 GameplayEffect 归类，无需通过 Boss HP 残差
推算。黯星同样可直接识别其结束结算伤害。

覆纹与浊燃不是同一反应。资源表中覆纹对应 Reaction 2，相关 GameplayEffect
索引包括新版 `552..556`，其中 `555` 为
`GE_ActorReaction_2_new_Damage`。本次抓包未出现这些索引，因此覆纹仍使用现有
残差推算。

### 角色攻击方式分类

每条伤害先按 bit alignment 关联同包的 `FHTClientActiveGE.UniqueIndex`，再依次查询：

```text
UniqueIndex
  -> gameplay_effect_mapping.json 的 GameplayEffect 名称
  -> skill_damage.json 的 DamageSourceCategory / GAName
```

资源表中的已验证分类如下：

| DamageSourceCategory | 程序显示 | 含义 |
|---|---|---|
| `A` | 普攻 | 普通攻击及资源表归入 A 类的攻击 |
| `E` | E技能 | 小技能 |
| `Q` | Q技能 | 大招 |
| `H` | 环合 | QTE / 入场攻击 |
| `R` | 环合伤害 | 属性反应伤害 |
| `Z` | 闪避反击 | 极限闪避后的反击 |

当分类字段缺失时，程序会根据 `GameplayEffect` / `GAName` 中的
`Melee`、`Skill`、`UltraSkill`、`QTE` 等稳定命名做降级分类；仍无法判断时显示
“其他”。如果一个伤害包中存在多个同位对齐的 GameplayEffect，则保持“未知”，
不强行归因。

2026-06-14 最新专项抓包 `nte_raw_20260614_025620_513.pcapng` 回放结果：

```text
总伤害记录 99
已分类      95
普攻        64
E技能        9
Q技能       13
QTE          1
闪避反击      4
覆纹          4
未知          4（均为 Boss 对角色的受击记录）
```

命中记录和 JSON 导出同时保存 `attack_type`、`gameplay_effect_index`、
`gameplay_effect_name`、`ability_name`、`damage_name`，便于继续细分具体招式段数。
其中 `damage_name` 来自
`res/data/skills/wooden_damage_descriptions.json` 的
`Desc.CultureInvariantString`，例如：

```text
GE_Player_Sagiri_QTE1_Damage   -> 早雾QTE
GE_Player_Sagiri_Skill5_Damage -> 早雾技能长按1
GE_Player_Nanally_Melee1_Damage -> 娜娜莉普攻1
```

Wooden 表缺少部分 Reaction 和新招式条目，因此攻击大类仍以
`skill_damage.json.DamageSourceCategory` 为准；Wooden 中文名缺失时保留
GameplayEffect/GAName 作为降级信息。

角色战斗明细优先显示 `damage_name`，因此可以直接区分“普攻第几段”、
“技能第几段”等具体招式。明细窗口支持按具体招式筛选，并按招式汇总输出次数、
总伤害以及该招式占角色总输出的比例：

```text
招式伤害占比 = 该招式累计伤害 / 角色累计输出伤害
```

汇总行可直接点击并切换为对应招式筛选，再次点击可取消筛选。

### QTE / 异能环合类型

QTE 是角色切换时触发的异能环合。程序记录 QTE 封包之前最近一次声明的活动角色，
以“前台角色属性 + 入场角色属性”判断本次环合，而不是仅按队伍中存在的属性猜测：

| 属性组合 | 环合类型 | 伤害处理 |
|---|---|---|
| 光 + 灵 | 创生 | 创生花伤害保留在触发角色名下，不额外生成独立记录 |
| 灵 + 咒 | 覆纹 | 后续追加攻击通过 `follow_up_damage` 合并到源命中 |
| 咒 + 暗 | 浊燃 | 持续伤害按 `Buff_Reaction_5_new` 直接识别 |
| 暗 + 魂 | 黯星 | 状态结束伤害按 Reaction 4 GameplayEffect 识别 |
| 光 + 相 | 延滞 | 控制效果，本身不额外计伤 |
| 魂 + 相 | 浸染 | 按对应 Reaction GameplayEffect 识别 |

底层 `attack_type` 保存为 `环合·浊燃`、`环合·覆纹` 等；若缺少前台角色声明或
属性，仍保守显示为 `环合`。

`盈蓄`、`失谐` 仍可通过 GameplayEffect 名称中的 Reaction 7/8 或 `AnHunZhou`
归类；当前代码不根据队伍三属性组合主动推断这两类环合。

## 角色声明

角色 ID 使用同一位流对齐方式搜索以下 9 字节结构：

```text
05 00 00 00 [四位 ASCII 数字] 00
```

单包只声明一个角色时直接关联；多角色声明包按每条伤害记录的 bit alignment
寻找唯一角色；仍无法消歧时使用同一 UDP 四元组最近一次确认的角色。

角色归属公式：

```text
char_id = 单角色声明ID
       ?? 与伤害记录同bit alignment的唯一声明ID
       ?? 同一UDP四元组最近确认ID
       ?? 0
```

其中 `??` 表示前者不存在时使用后者。

## JSON / PCAPNG 回放

实时抓包、PCAPNG 回放和 JSON 回放都通过 `EngineEvent` 写入同一 `CombatState`。
PCAPNG 回放复用实时解析器，区别是可以传入本机 IP hint；无本机 IP 时使用方向
启发式。JSON 回放会重放导出的 `packets` 和 `hits`：

- `ExportHit.direction` 缺失时默认按旧 JSON 兼容为 `outgoing`。
- `target_id`、`target_name`、`target_context` 只作为旧 JSON 兼容字段导入导出，
  稳定主线不主动填充或显示敌方目标识别。
- 旧导出中的 `attack_type = "QTE·xxx"` 会在导入时转换为 `环合·xxx`。
- `follow_up_damage`、`follow_up_attack_type`、`follow_up_damage_attribute` 等字段
  带 `serde(default)`，旧 JSON 缺失时可正常导入。

## CurrentHP 状态复制

2026-06-13 的深渊受击与自动回血抓包中，服务器到客户端的 CurrentHP 更新可在任意
bit alignment 下识别为以下 16 字节前缀加一个 `float32`：

```text
?? 00 00 e0 4f 33 33 ?? 0f ?? ?? 00 00 00 00 24 [CurrentHP:f32]
```

同一角色的已验证序列为：

```text
21198 -> 21460 -> 21998 -> 19172 -> 19427 -> 19637
      -> 20029 -> 20513 -> 20946
```

其中第一次受击后发生两次回血更新：`+262` 和 `+538`，合计 `800`。该结构本身没有
角色 ID，因此当前实现只提取 CurrentHP 候选并写入 Debug 包说明，不直接计入角色治疗；
可靠归属仍需补全 NetRefHandle/对象实例映射。

## Boss HP 状态复制

服务器到客户端的 Boss HP 更新使用以下结构：

```text
06 00 00 00 00 20 00 00
[对象/属性实例 GUID:16 bytes]
00 00 00 00 00 00 00 00 00 00 00 00
[CurrentHP:f32]
```

总前缀长度为 36 字节，之后紧跟当前 HP 的 `float32`。

GUID 会随战斗、Boss 或属性实例变化，不能写死。当前解析条件为：

1. 前 8 字节严格等于 `06 00 00 00 00 20 00 00`。
2. 中间 16 字节 GUID 不能全为 0。
3. GUID 后 12 字节必须全为 0。
4. HP 必须是有限数，且满足 `0 <= HP <= 1e9`。
5. 对 UDP 载荷的每个字节位置和 `bit_shift=0..7` 扫描。

一场战斗的服务器实际掉血为：

```text
Boss实际掉血 = HP序列最大值 - HP序列最小值
```

若已捕获战斗开始和结束，也可写作：

```text
Boss实际掉血 = Boss初始HP - Boss最终HP
```

Boss 最终可能停留在 `1`，因此不能假设击杀后一定回传 `0`。

### 服务端伤害校准

Debug 面板中的“服务端伤害校准”启用后，Boss HP 更新会用于修正单条待确认输出：

```text
窗口 = 1.0 秒
按 target_handle 分别记录上一条 Boss HP
候选命中条件：
  previous_hp_timestamp < hit.timestamp <= current_hp_timestamp
  hit.direction != incoming
  hit.target_max_hp > 0
候选数必须恰好为 1
修正伤害 = previous_hp - current_hp
```

若 `current_hp <= 1`，实现按 `0` 处理，以适配部分 Boss 死亡时最终 HP 停在 1 的
回包。校准成功后产生 `HitDamageCorrection`，替换源命中的 `damage`、
`target_hp_before`、`target_hp_after` 和 `target_hp_percent`，并重建总览、
角色与深渊统计。

未开启服务端伤害校准时，Boss HP 降到 `0/1` 且最近确认命中尚未反映该尾段掉血时，
会生成 `HP同步伤害` follow-up，用于补齐击杀尾伤。

## 基础伤害与 HP

每条解析伤害记录使用以下派生值：

```text
封包伤害 = F0
封包预测HP = max(F1 - F0, 0)
封包HP百分比 = 封包预测HP / F2 * 100%
```

其中：

- `F1` 是该段伤害结算前的目标 HP。
- `F2` 是目标最大 HP。
- `F0` 与 `F5` 必须在容差内相等。

重复伤害字段校验容差为：

```text
tolerance = max(0.01, abs(F0) * 1e-6)
accept when abs(F0 - F5) <= tolerance
```

基础伤害有效范围：

```text
2 <= F0 <= 1e9
F1 >= 0
F2 > 0
```

## 覆纹伤害解析

### 已确认机制

1. 覆纹按每一段伤害单独结算。
2. 一次攻击有 10 段时，最多对应 10 段覆纹。
3. 覆纹紧跟对应的原始伤害，不应跨段重新排序匹配。
4. 文本描述为追加原伤害的 20%。
5. 只有队伍同时存在“灵”和“咒”属性角色时才可能触发。
6. 效果持续游戏内 12 秒；时停会使现实持续时间超过 12 秒。

当前实现通过两类证据打开覆纹追踪窗口：

```text
1. 已解析输出命中的 attack_type == "环合·覆纹"
2. S2C 包在固定偏移和 bit shift 下出现覆纹启动候选：
   signature @ byte 22 / bit 3 = 01 00 00 00 02 00 00 00
   入场角色 ID @ byte 53 / bit 0
   前台角色 ID @ byte 66 / bit 2
```

第二类证据只用于记录“灵 + 咒”角色组合和 Debug 备注；真正开始记录覆纹追加伤害
仍要求后续可见的 `环合·覆纹` 命中。游戏内 12 秒持续时间尚未参与过滤。

### FIFO 逐段匹配

输出伤害按发生时间进入 FIFO 队列。服务器 Boss HP 更新到达时，只取队首伤害
进行结算，符合“覆纹紧跟每段原伤害”的机制。

网络关联窗口为 1 秒：

```text
保留条件：HP回包时间 - 伤害包时间 <= 1.0 秒
```

这里的 1 秒仅用于关联客户端伤害与服务器 HP 回包，不是覆纹持续时间。

若连续两条伤害包间隔超过 1 秒，则清理尚未结算的旧队列，避免跨技能或跨战斗
错误关联。

对每一条服务器 HP 更新：

```text
实际掉血 = 上一次服务器HP - 当前服务器HP
残差伤害 = 实际掉血 - 队首封包伤害
残差比例 = 残差伤害 / 队首封包伤害
```

当前覆纹接受条件：

```text
已激活覆纹窗口
本场已观察到至少一个灵属性角色
本场已观察到至少一个咒属性角色
当前队首原始伤害角色属性属于灵或咒
该原始伤害的 attack_type 不是创生花、覆纹、延滞、黯星、浊燃、浸染、盈蓄、失谐
该原始伤害的 attack_type 若以 "环合·" 开头，则必须等于 "环合·覆纹"
残差伤害 >= 1
```

接受后不再生成独立命中记录，而是生成 `HitFollowUp` 并合并回源命中：

```text
follow_up_damage           = 残差伤害
follow_up_damage_name      = 覆纹追加攻击
follow_up_attack_type      = 覆纹
follow_up_damage_attribute = 源命中角色属性
target_hp_after            = 当前服务器HP
target_hp_percent          = 当前服务器HP / 源命中 target_max_hp * 100%
```

这样 UI 和导出 JSON 中仍保留原始命中，同时通过 `Hit.total_damage()` 把本体伤害
和覆纹追加伤害一起计入总伤害、角色统计、技能汇总和深渊上下行线统计。

光、魂、暗、相属性角色的伤害仍会进入 FIFO 并消耗对应的服务器 HP 更新，以维持
逐段结算顺序，但无论残差是否存在，都不会生成覆纹追加伤害。

当前队伍属性通过 C2S 角色声明和已解析输出角色累计确认。检测到新目标最大 HP、
Boss 大幅回满，或长时间间隔后的满血新战斗时，会清空上一场的队伍属性，避免
跨战斗状态污染。

历史样本中曾使用过 18% 到 26% 的识别置信区间；当前生产实现已取消比例门槛，
改为依赖显式覆纹激活证据、队伍属性、源命中属性和服务器 HP FIFO 顺序来降低误判。

### 为什么必须使用 FIFO

旧算法从待处理队列中选择“数值最接近实际掉血”的伤害。在多段攻击并发时，
某次 HP 更新可能更接近另一段伤害，导致跨段抢配和后续连锁漏记。

2026-06-13 21:36 抓包对比：

```text
Boss实际掉血                3,954,534
原始封包伤害                3,785,075
数值最接近算法的覆纹          141,672
数值最接近算法仍缺             27,787

FIFO逐段算法的覆纹            169,789
FIFO合计伤害                3,954,864
FIFO与Boss掉血差异                330
相对误差                     0.008%
```

因此生产实现采用 FIFO 逐段匹配。

### 理论覆纹公式

若已知客户端显示的原始伤害：

```text
理论覆纹 = floor(原始显示伤害 * 0.20)
理论总伤害 = 原始显示伤害 + 理论覆纹
```

示例：

```text
原始显示伤害 = 3209
覆纹显示伤害 = floor(3209 * 0.20) = 641
总掉血       = 3209 + 641 = 3850
```

但封包中该次基础伤害为 `3177`，与显示值 `3209` 相差 32，因此不能直接用
`封包伤害 * 20%` 精确还原飘字。对于已验证的娜娜莉普攻一段
`GE_Player_Nanally_Melee1_Damage`（UniqueIndex 241），可从服务器总掉血反解：

```text
服务器实际掉血 = 3850
封包伤害       = 3177
求整数 D，使 D + floor(D * 20%) = 3850
显示原伤害 D   = 3209
覆纹显示伤害   = 641
```

只有反解出的显示原伤害与封包伤害偏差不超过 3% 时才接受该修正。其他效果仍使用
服务器 HP 残差，避免把普通伤害波动误判为覆纹。

### 2026-06-14 覆纹专项抓包

`nte_raw_20260614_024554_403.pcapng` 中主动触发了多轮覆纹。生产解析得到：

```text
覆纹段数       = 196
覆纹总伤害     = 454,264
首段时间       = 1781376406.341029
末段时间       = 1781376510.147382
残差比例范围   = 19.8% .. 23.4%
```

样本包含单段、连续多段和同一服务器 HP 回包结算多段伤害的情况，均能按 FIFO
顺序关联到娜娜莉的灵属性原始伤害。原始包中直接搜索 `552..556` 会在大量普通
伤害和状态数据中产生碰撞，不能脱离字段边界作为覆纹状态依据；当前可靠解析依据
仍是服务器确认的逐段 HP 残差。

`nte_raw_20260614_025620_513.pcapng` 的四次人工核对结果为：

```text
早雾 1714：覆纹 401
早雾 3481：覆纹 815
娜娜莉封包 3177 / 显示 3209：覆纹 641
娜娜莉封包 2898 / 显示 2848：覆纹 569
```

修正后程序依次输出 `401、815、641、569`。

## 统计公式

### 总伤害与总承伤

```text
总伤害 = Σ 所有非 incoming 记录的 total_damage()
总承伤 = Σ 所有 incoming 记录的 total_damage()
```

覆纹追加伤害和 HP 同步补伤通过 `follow_up_damage` 合并到源命中；统计使用
`Hit.total_damage() = damage + follow_up_damage`，因此追加伤害会计入总伤害、
角色统计、技能汇总和战斗时间。服务端伤害校准开启时，会用服务器 HP 差值修正
源命中的 `damage`，而不是生成 follow-up。

### 角色统计

```text
角色伤害 = Σ 该角色所有非 incoming total_damage
角色命中数 = 该角色非 incoming 记录数
角色受击 = Σ 该角色所有 incoming total_damage
角色受击次数 = 该角色 incoming 记录数
```

角色输出持续时间：

```text
若命中数 > 1：
    duration = max(最后命中时间 - 首次命中时间, 0.001)
否则：
    duration = 0
```

角色 DPS：

```text
角色DPS = 角色伤害 / max(duration, 1.0)
```

### 队伍统计

队伍战斗时间：

```text
duration = max(最后一条输出时间 - 第一条输出时间, 0.001)
```

队伍 DPS：

```text
队伍DPS = 总伤害 / max(duration, 1.0)
```

伤害占比：

```text
角色占比 = 角色伤害 / 队伍总伤害 * 100%
```

### 深渊上下半

深渊事件将输出记录分配到当前激活的上半或下半。重新挑战后若新的阶段事件在
10 秒窗口内切换到另一半，会按重开逻辑清理对应统计：

```text
重开阶段关联条件：
0 <= 阶段事件时间 - 重开检测时间 <= 10 秒
```

该 10 秒同样是事件关联窗口，不是游戏战斗计时。

## 角色属性

`res/data/characters/characters.json` 的角色记录可包含：

```json
{
  "attribute": "灵"
}
```

允许值固定为：

```text
灵、咒、光、魂、暗、相
```

属性图标映射：

| 属性 | 资源 |
|---|---|
| 灵 | `res/images/attributes/UI_avatarbg_Icon_01.png` |
| 咒 | `res/images/attributes/UI_avatarbg_Icon_06.png` |
| 光 | `res/images/attributes/UI_avatarbg_Icon_04.png` |
| 魂 | `res/images/attributes/UI_avatarbg_Icon_05.png` |
| 暗 | `res/images/attributes/UI_avatarbg_Icon_03.png` |
| 相 | `res/images/attributes/UI_avatarbg_Icon_02.png` |

属性字段目前用于 UI 展示和后续覆纹触发条件判断。未设置属性时保持兼容，并在
队伍列表中使用排名数字作为图标兜底。

## 已知限制

- 已确认 `Buff_Reaction_5_new`（UniqueIndex 561）是浊燃而非覆纹；Reaction 2
  数值在普通伤害包中存在大量字段碰撞，尚不能作为独立状态锚点。覆纹实际伤害
  仍通过服务器 HP 残差解析。
- “灵 + 咒”队伍条件依赖 `res/data/characters/characters.json`
  属性数据以及抓包中已观察到的角色；
  属性缺失或角色尚未出现在角色声明/伤害包中时会保守地不记录覆纹。
- 游戏内 12 秒覆纹持续窗口尚无可靠网络时钟；`damage_time` 与 `world_time` 在
  当前样本中几乎与现实抓包时间同步，无法证明它们会在时停期间暂停。
- 多次服务器/客户端计算及取整会产生小误差。最新完整样本累计差异为 330，
  相对 Boss 总掉血约 0.008%。
- 低血量敌人和高血量玩家的严格伤害方向仍需要对象实例映射。

## 逆向依据

- 旧提交中的真实 `pcapng` 和 JSONL 日志验证了字段头、长度和数值。
- 真实记录的 F3 与 F4 随抓包时间同步递增。
- F1/F2 与目标血量变化连续对应，F0/F5 始终重复。
- IDA MCP 在游戏客户端中确认了 `DamageValue`、`DamageTime`、`CurrentHp`、
  `MaxHPTemp`、`ClientOnSendHandleDamageInfoToInstigator`、
  `ClientOnSendHandleDamageInfoToDamaged` 等反射名称。
- IDA 中同时存在 Unreal Iris `NetSerializer` 和 bitstream 实现，符合任意位对齐
  的网络序列化特征。
