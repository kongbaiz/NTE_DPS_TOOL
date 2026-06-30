use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

const ABYSS_RESTART_STAGE_WINDOW_SECONDS: f64 = 10.0;

use serde::{Deserialize, Serialize};

const MAX_COMBAT_HITS: usize = 50_000;
/// Low-water mark for trimming. Once the hit window exceeds `MAX_COMBAT_HITS` we trim down to this
/// in one pass and rebuild aggregates once, instead of popping a single hit and rebuilding on every
/// subsequent push. This turns the O(n) rebuild from per-hit into amortized O(1) at the cap while
/// keeping memory bounded by `MAX_COMBAT_HITS`.
const COMBAT_HITS_RETAIN: usize = 46_000;

/// Compact, exportable team DPS snapshot used to predict abyss clear time.
/// Deliberately holds no packets or per-hit data — only the latest total DPS and
/// up to 4 members — so the exported file stays tiny.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TeamDps {
    pub dps: f64,
    #[serde(default)]
    pub members: Vec<TeamDpsMember>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TeamDpsMember {
    pub id: u32,
    pub dps: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// On-disk "team DPS data" file: a single team and/or the abyss upper/lower
/// teams. Every field is optional so the same format covers single-team and
/// dual-team (abyss) exports. Serialized compactly (no pretty-printing).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TeamDpsExport {
    #[serde(default = "team_dps_export_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single: Option<TeamDps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper: Option<TeamDps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower: Option<TeamDps>,
}

pub const TEAM_DPS_EXPORT_VERSION: u32 = 1;
pub const TEAM_DPS_MAX_MEMBERS: usize = 4;

fn team_dps_export_version() -> u32 {
    TEAM_DPS_EXPORT_VERSION
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CharacterInfo {
    #[serde(default)]
    pub name_zh: String,
    #[serde(default)]
    pub name_en: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub attribute: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub timestamp: f64,
    pub char_id: u32,
    pub char_name: String,
    pub char_known: bool,
    pub damage: f64,
    pub byte_offset: usize,
    pub bit_shift: u8,
    pub char_source: String,
    pub direction: String,
    pub target_hp_before: f64,
    pub target_hp_after: f64,
    pub target_max_hp: f64,
    pub target_hp_percent: f64,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub target_name: Option<String>,
    #[serde(default)]
    pub target_context: Vec<String>,
    #[serde(default)]
    pub gameplay_effect_index: Option<u32>,
    #[serde(default)]
    pub gameplay_effect_name: Option<String>,
    #[serde(default)]
    pub ability_name: Option<String>,
    #[serde(default)]
    pub damage_name: Option<String>,
    #[serde(default)]
    pub attack_type: Option<String>,
    #[serde(default)]
    pub damage_attribute: Option<String>,
    #[serde(default)]
    pub follow_up_damage: f64,
    #[serde(default)]
    pub follow_up_timestamp: Option<f64>,
    #[serde(default)]
    pub follow_up_damage_name: Option<String>,
    #[serde(default)]
    pub follow_up_attack_type: Option<String>,
    #[serde(default)]
    pub follow_up_damage_attribute: Option<String>,
}

impl Hit {
    pub fn total_damage(&self) -> f64 {
        self.damage + self.follow_up_damage
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HitFollowUp {
    pub source_timestamp: f64,
    pub source_char_id: u32,
    pub source_damage: f64,
    pub source_target_hp_before: f64,
    pub source_target_hp_after: f64,
    pub source_target_max_hp: f64,
    #[serde(default)]
    pub source_gameplay_effect_index: Option<u32>,
    pub timestamp: f64,
    pub damage: f64,
    pub target_hp_after: f64,
    pub target_hp_percent: f64,
    #[serde(default)]
    pub damage_name: Option<String>,
    #[serde(default)]
    pub attack_type: Option<String>,
    #[serde(default)]
    pub damage_attribute: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HitDamageCorrection {
    pub source_timestamp: f64,
    pub source_char_id: u32,
    pub source_damage: f64,
    pub source_target_hp_before: f64,
    pub source_target_hp_after: f64,
    pub source_target_max_hp: f64,
    #[serde(default)]
    pub source_gameplay_effect_index: Option<u32>,
    pub damage: f64,
    pub target_hp_before: f64,
    pub target_hp_after: f64,
    pub target_hp_percent: f64,
}

#[derive(Clone, Debug)]
pub struct PacketDebug {
    pub timestamp: f64,
    pub source: String,
    pub destination: String,
    pub direction: String,
    pub payload_len: usize,
    pub declared_ids: Vec<u32>,
    pub parsed_hits: usize,
    pub note: String,
    pub payload_preview: String,
    pub payload_hex: String,
    pub decoded_text: String,
}

#[derive(Clone, Debug, Default)]
pub struct CharacterStats {
    pub char_id: u32,
    pub name: String,
    pub hits: u64,
    pub damage: f64,
    pub hits_taken: u64,
    pub damage_taken: f64,
    pub first_hit: f64,
    pub last_hit: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HitDirectionSummary {
    pub outgoing_damage: f64,
    pub outgoing_hits: u64,
    pub unknown_damage: f64,
    pub unknown_hits: u64,
    pub incoming_damage: f64,
    pub incoming_hits: u64,
}

impl HitDirectionSummary {
    pub fn unknown_share(&self) -> f64 {
        let total_output = self.outgoing_damage + self.unknown_damage;
        if total_output > 0.0 {
            self.unknown_damage / total_output * 100.0
        } else {
            0.0
        }
    }
}

pub fn summarize_hit_directions<'a>(
    hits: impl IntoIterator<Item = &'a Hit>,
) -> HitDirectionSummary {
    let mut summary = HitDirectionSummary::default();
    for hit in hits {
        let damage = hit.total_damage();
        match hit.direction.as_str() {
            "incoming" => {
                summary.incoming_damage += damage;
                summary.incoming_hits += 1;
            }
            "outgoing" => {
                summary.outgoing_damage += damage;
                summary.outgoing_hits += 1;
            }
            _ => {
                summary.unknown_damage += damage;
                summary.unknown_hits += 1;
            }
        }
    }
    summary
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TimelineTimeStopInterval {
    pub start_offset: f64,
    pub end_offset: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineRoleBucket {
    pub char_id: u32,
    pub char_name: String,
    pub damage: f64,
    pub dps: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineBucket {
    pub start_offset: f64,
    pub end_offset: f64,
    pub damage: f64,
    pub dps: f64,
    pub cumulative_damage: f64,
    pub hits: u64,
    pub role_damage: Vec<TimelineRoleBucket>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineMarkerKind {
    HalfStart,
    Clear,
    Exit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimelineMarker {
    pub offset: f64,
    pub label: String,
    pub kind: TimelineMarkerKind,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineSeries {
    pub bucket_seconds: f64,
    pub start_timestamp: Option<f64>,
    pub end_timestamp: Option<f64>,
    pub total_damage: f64,
    pub buckets: Vec<TimelineBucket>,
    pub time_stop_intervals: Vec<TimelineTimeStopInterval>,
    pub markers: Vec<TimelineMarker>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillBreakdown {
    pub total_damage: f64,
    pub total_hits: u64,
    pub rows: Vec<SkillBreakdownRow>,
    pub unknown: UnknownAttributionSummary,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkillBreakdownRow {
    pub char_id: u32,
    pub char_name: String,
    pub name: String,
    pub category: String,
    pub ability_name: Option<String>,
    pub damage_name: Option<String>,
    pub gameplay_effect_index: Option<u32>,
    pub gameplay_effect_name: Option<String>,
    pub is_follow_up: bool,
    pub hits: u64,
    pub damage: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnknownAttributionSummary {
    pub unknown_character_count: usize,
    pub unknown_character_hits: u64,
    pub unknown_direction_hits: u64,
    pub unknown_direction_damage: f64,
    pub unmapped_skill_rows: usize,
    pub unmapped_skill_hits: u64,
    pub unmapped_skill_damage: f64,
    pub unmapped_gameplay_effects: Vec<UnknownGameplayEffect>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnknownGameplayEffect {
    pub index: u32,
    pub hits: u64,
    pub damage: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureQualitySource {
    Live,
    PcapngReplay,
    JsonReplay,
    #[default]
    Unknown,
}

impl CaptureQualitySource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "实时抓包",
            Self::PcapngReplay => "PCAPNG 回放",
            Self::JsonReplay => "JSON 回放",
            Self::Unknown => "当前会话",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureQualitySummary {
    pub source: CaptureQualitySource,
    pub packet_count: usize,
    pub packets_with_hits: usize,
    pub hit_count: usize,
    pub outgoing_hits: u64,
    pub outgoing_damage: f64,
    pub unknown_direction_hits: u64,
    pub unknown_direction_damage: f64,
    pub incoming_hits: u64,
    pub incoming_damage: f64,
    pub unknown_character_count: usize,
    pub unknown_character_hits: u64,
    pub unmapped_skill_rows: usize,
    pub unmapped_skill_hits: u64,
    pub unmapped_gameplay_effect_count: usize,
    pub time_stop_event_count: u64,
    pub time_stop_interval_count: usize,
    pub abyss_event_count: u64,
    pub server_damage_corrections: u64,
}

impl CaptureQualitySummary {
    pub fn redacted_text(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "NTE DPS TOOL 解析质量报告");
        let _ = writeln!(text, "统计来源：{}", self.source.label());
        let _ = writeln!(
            text,
            "封包：{} 个（含命中 {} 个）",
            self.packet_count, self.packets_with_hits
        );
        let _ = writeln!(text, "命中：{} 条", self.hit_count);
        let _ = writeln!(
            text,
            "方向：输出 {} 条 / 候选 {} 条 / 受击 {} 条",
            self.outgoing_hits, self.unknown_direction_hits, self.incoming_hits
        );
        let _ = writeln!(
            text,
            "伤害：输出 {:.0} / 候选 {:.0} / 受击 {:.0}",
            self.outgoing_damage, self.unknown_direction_damage, self.incoming_damage
        );
        let _ = writeln!(
            text,
            "未知角色：{} 个，{} 条命中",
            self.unknown_character_count, self.unknown_character_hits
        );
        let _ = writeln!(
            text,
            "待映射技能：{} 类，{} 条命中",
            self.unmapped_skill_rows, self.unmapped_skill_hits
        );
        let _ = writeln!(
            text,
            "未映射 GE：{} 个",
            self.unmapped_gameplay_effect_count
        );
        let _ = writeln!(
            text,
            "时停事件：{} 个，合并区间 {} 段",
            self.time_stop_event_count, self.time_stop_interval_count
        );
        let _ = writeln!(text, "深渊事件：{} 个", self.abyss_event_count);
        let _ = write!(
            text,
            "服务端伤害校准：{} 条",
            self.server_damage_corrections
        );
        text
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CombatSessionSummary {
    pub duration_seconds: f64,
    pub dps_time_mode: String,
    pub total_damage: f64,
    pub total_dps: f64,
    pub total_damage_taken: f64,
    pub total_hits: u64,
    pub characters: Vec<CombatSessionCharacterSummary>,
    pub skills: Vec<CombatSessionSkillSummary>,
    pub abyss: CombatSessionAbyssSummary,
    pub quality: CaptureQualitySummary,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CombatSessionCharacterSummary {
    pub char_id: u32,
    pub name: String,
    pub hits: u64,
    pub damage: f64,
    pub dps: f64,
    pub damage_share_percent: f64,
    pub hits_taken: u64,
    pub damage_taken: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CombatSessionSkillSummary {
    pub char_id: u32,
    pub char_name: String,
    pub name: String,
    pub category: String,
    pub hits: u64,
    pub damage: f64,
    pub damage_share_percent: f64,
    pub is_follow_up: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CombatSessionAbyssSummary {
    pub detected: bool,
    pub floor: Option<u32>,
    pub active_half: Option<String>,
    pub success: bool,
    pub first_half: Option<CombatSessionAbyssHalfSummary>,
    pub second_half: Option<CombatSessionAbyssHalfSummary>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CombatSessionAbyssHalfSummary {
    pub half: String,
    pub duration_seconds: f64,
    pub total_damage: f64,
    pub total_dps: f64,
    pub characters: Vec<CombatSessionCharacterSummary>,
    pub skills: Vec<CombatSessionSkillSummary>,
}

#[allow(dead_code)]
pub fn summarize_timeline<'a>(
    hits: impl IntoIterator<Item = &'a Hit>,
    bucket_seconds: f64,
) -> TimelineSeries {
    summarize_timeline_with_time_stop(
        hits,
        &TimeStopTracker::default(),
        Vec::new(),
        bucket_seconds,
        false,
    )
}

/// Default idle span (no outgoing damage) that separates one capture into
/// distinct combat segments.
pub const COMBAT_SEGMENT_GAP_SECONDS: f64 = 5.0;

/// One detected stretch of sustained combat within a capture. Offsets are
/// relative to the timeline start, matching [`TimelineSeries`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CombatSegment {
    pub start_offset: f64,
    pub end_offset: f64,
    pub duration: f64,
    pub total_damage: f64,
    pub hits: u64,
    pub dps: f64,
}

/// Split a [`TimelineSeries`] into combat segments separated by idle gaps longer
/// than `gap_seconds`. Derived from the already-aggregated buckets, so per-segment
/// damage matches the chart and the team totals exactly. This is purely
/// read-only — it never resets or mutates live combat state.
pub fn summarize_combat_segments(series: &TimelineSeries, gap_seconds: f64) -> Vec<CombatSegment> {
    let bucket_seconds = if series.bucket_seconds.is_finite() && series.bucket_seconds > 0.0 {
        series.bucket_seconds
    } else {
        1.0
    };
    let gap_seconds = if gap_seconds.is_finite() && gap_seconds > 0.0 {
        gap_seconds
    } else {
        COMBAT_SEGMENT_GAP_SECONDS
    };
    let gap_buckets = (gap_seconds / bucket_seconds).ceil().max(1.0) as usize;

    let mut segments: Vec<CombatSegment> = Vec::new();
    let mut current: Option<CombatSegment> = None;
    let mut empty_run = 0usize;
    for bucket in &series.buckets {
        let active = bucket.hits > 0 && bucket.damage > 0.0;
        if active {
            if empty_run >= gap_buckets
                && let Some(segment) = current.take()
            {
                segments.push(segment);
            }
            empty_run = 0;
            let segment = current.get_or_insert(CombatSegment {
                start_offset: bucket.start_offset,
                ..CombatSegment::default()
            });
            segment.end_offset = bucket.end_offset;
            segment.total_damage += bucket.damage;
            segment.hits += bucket.hits;
        } else {
            empty_run += 1;
        }
    }
    if let Some(segment) = current.take() {
        segments.push(segment);
    }
    for segment in &mut segments {
        segment.duration = (segment.end_offset - segment.start_offset).max(0.0);
        segment.dps = if segment.duration > 0.0 {
            segment.total_damage / segment.duration
        } else {
            0.0
        };
    }
    segments
}

pub fn summarize_skill_breakdown<'a>(
    hits: impl IntoIterator<Item = &'a Hit>,
    char_filter: Option<u32>,
) -> SkillBreakdown {
    let mut rows = HashMap::<SkillBreakdownKey, SkillBreakdownRow>::new();
    let mut unknown_characters = HashSet::<u32>::new();
    let mut unknown = UnknownAttributionSummary::default();
    let mut unmapped_gameplay_effects = HashMap::<u32, UnknownGameplayEffect>::new();
    let mut total_damage = 0.0;
    let mut total_hits = 0;

    for hit in hits.into_iter().filter(|hit| {
        hit.direction != "incoming" && char_filter.is_none_or(|char_id| hit.char_id == char_id)
    }) {
        if !hit.char_known {
            unknown_characters.insert(hit.char_id);
            unknown.unknown_character_hits += 1;
        }
        if hit.direction == "unknown" {
            unknown.unknown_direction_hits += 1;
            unknown.unknown_direction_damage += hit.total_damage();
        }

        if hit.damage > 0.0 {
            let entry = SkillEntryRef::from_hit(hit, false);
            push_skill_breakdown_entry(&mut rows, hit, entry, hit.damage);
            total_damage += hit.damage;
            total_hits += 1;
            observe_unknown_skill(
                hit,
                hit.damage,
                &mut unknown,
                &mut unmapped_gameplay_effects,
            );
        }
        if hit.follow_up_damage > 0.0 {
            let entry = SkillEntryRef::from_hit(hit, true);
            push_skill_breakdown_entry(&mut rows, hit, entry, hit.follow_up_damage);
            total_damage += hit.follow_up_damage;
            total_hits += 1;
        }
    }

    let mut sorted_rows = rows.into_values().collect::<Vec<_>>();
    sorted_rows.sort_by(|left, right| {
        right
            .damage
            .total_cmp(&left.damage)
            .then_with(|| left.char_name.cmp(&right.char_name))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.is_follow_up.cmp(&right.is_follow_up))
    });
    unknown.unknown_character_count = unknown_characters.len();
    unknown.unmapped_skill_rows = sorted_rows
        .iter()
        .filter(|row| is_unmapped_skill_row(row))
        .count();
    unknown.unmapped_gameplay_effects = unmapped_gameplay_effects.into_values().collect();
    unknown
        .unmapped_gameplay_effects
        .sort_by_key(|effect| effect.index);

    SkillBreakdown {
        total_damage,
        total_hits,
        rows: sorted_rows,
        unknown,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SkillBreakdownKey {
    char_id: u32,
    name: String,
    category: String,
    ability_name: Option<String>,
    damage_name: Option<String>,
    gameplay_effect_index: Option<u32>,
    gameplay_effect_name: Option<String>,
    is_follow_up: bool,
}

struct SkillEntryRef {
    name: String,
    category: String,
    ability_name: Option<String>,
    damage_name: Option<String>,
    gameplay_effect_index: Option<u32>,
    gameplay_effect_name: Option<String>,
    is_follow_up: bool,
}

impl SkillEntryRef {
    fn from_hit(hit: &Hit, is_follow_up: bool) -> Self {
        if is_follow_up {
            return Self {
                name: hit
                    .follow_up_damage_name
                    .as_deref()
                    .or(hit.follow_up_attack_type.as_deref())
                    .unwrap_or("后续伤害")
                    .to_owned(),
                category: hit
                    .follow_up_attack_type
                    .clone()
                    .unwrap_or_else(|| "后续伤害".to_owned()),
                ability_name: hit.ability_name.clone(),
                damage_name: hit.follow_up_damage_name.clone(),
                gameplay_effect_index: hit.gameplay_effect_index,
                gameplay_effect_name: hit.gameplay_effect_name.clone(),
                is_follow_up,
            };
        }

        Self {
            name: hit
                .damage_name
                .as_deref()
                .or(hit.ability_name.as_deref())
                .or(hit.gameplay_effect_name.as_deref())
                .or(hit.attack_type.as_deref())
                .unwrap_or("待映射技能")
                .to_owned(),
            category: hit
                .attack_type
                .clone()
                .unwrap_or_else(|| "未归类".to_owned()),
            ability_name: hit.ability_name.clone(),
            damage_name: hit.damage_name.clone(),
            gameplay_effect_index: hit.gameplay_effect_index,
            gameplay_effect_name: hit.gameplay_effect_name.clone(),
            is_follow_up,
        }
    }
}

fn push_skill_breakdown_entry(
    rows: &mut HashMap<SkillBreakdownKey, SkillBreakdownRow>,
    hit: &Hit,
    entry: SkillEntryRef,
    damage: f64,
) {
    if !damage.is_finite() || damage <= 0.0 {
        return;
    }
    let keep_gameplay_effect_key = entry.damage_name.is_none() && entry.ability_name.is_none();
    let key = SkillBreakdownKey {
        char_id: hit.char_id,
        name: entry.name,
        category: entry.category,
        ability_name: entry.ability_name,
        damage_name: entry.damage_name,
        gameplay_effect_index: if keep_gameplay_effect_key {
            entry.gameplay_effect_index
        } else {
            None
        },
        gameplay_effect_name: if keep_gameplay_effect_key {
            entry.gameplay_effect_name
        } else {
            None
        },
        is_follow_up: entry.is_follow_up,
    };
    let row_key = key.clone();
    let row = rows.entry(key).or_insert_with(move || SkillBreakdownRow {
        char_id: hit.char_id,
        char_name: hit.char_name.clone(),
        name: row_key.name,
        category: row_key.category,
        ability_name: row_key.ability_name,
        damage_name: row_key.damage_name,
        gameplay_effect_index: row_key.gameplay_effect_index,
        gameplay_effect_name: row_key.gameplay_effect_name,
        is_follow_up: row_key.is_follow_up,
        hits: 0,
        damage: 0.0,
    });
    row.char_name.clone_from(&hit.char_name);
    row.hits += 1;
    row.damage += damage;
}

fn observe_unknown_skill(
    hit: &Hit,
    damage: f64,
    unknown: &mut UnknownAttributionSummary,
    unmapped_gameplay_effects: &mut HashMap<u32, UnknownGameplayEffect>,
) {
    if is_hit_skill_unmapped(hit) {
        unknown.unmapped_skill_hits += 1;
        unknown.unmapped_skill_damage += damage;
    }
    if let Some(index) = hit.gameplay_effect_index
        && hit.gameplay_effect_name.is_none()
    {
        let effect =
            unmapped_gameplay_effects
                .entry(index)
                .or_insert_with(|| UnknownGameplayEffect {
                    index,
                    ..Default::default()
                });
        effect.hits += 1;
        effect.damage += damage;
    }
}

fn is_hit_skill_unmapped(hit: &Hit) -> bool {
    hit.damage_name.is_none() && hit.ability_name.is_none() && hit.gameplay_effect_name.is_none()
}

fn is_unmapped_skill_row(row: &SkillBreakdownRow) -> bool {
    !row.is_follow_up
        && row.damage_name.is_none()
        && row.ability_name.is_none()
        && row.gameplay_effect_name.is_none()
}

impl CharacterStats {
    pub fn duration(&self) -> f64 {
        if self.hits > 1 {
            (self.last_hit - self.first_hit).max(0.001)
        } else {
            0.0
        }
    }
}

fn update_combat_totals(
    stats: &mut HashMap<u32, CharacterStats>,
    started_at: &mut Option<f64>,
    ended_at: &mut Option<f64>,
    total_damage: &mut f64,
    total_damage_taken: &mut f64,
    hit: &Hit,
) {
    let row = stats.entry(hit.char_id).or_insert_with(|| CharacterStats {
        char_id: hit.char_id,
        name: hit.char_name.clone(),
        first_hit: hit.timestamp,
        last_hit: hit.timestamp,
        ..Default::default()
    });
    row.name.clone_from(&hit.char_name);
    let damage = hit.total_damage();
    if hit.direction == "incoming" {
        row.hits_taken += 1;
        row.damage_taken += damage;
        *total_damage_taken += damage;
        return;
    }

    *started_at = Some(started_at.map_or(hit.timestamp, |value| value.min(hit.timestamp)));
    *ended_at = Some(ended_at.map_or(hit.timestamp, |value| value.max(hit.timestamp)));
    *total_damage += damage;
    if row.hits == 0 {
        row.first_hit = hit.timestamp;
        row.last_hit = hit.timestamp;
    } else {
        row.first_hit = row.first_hit.min(hit.timestamp);
        row.last_hit = row.last_hit.max(hit.timestamp);
    }
    row.hits += 1;
    row.damage += damage;
}

fn rebuild_combat_totals(
    hits: &VecDeque<Hit>,
    stats: &mut HashMap<u32, CharacterStats>,
    started_at: &mut Option<f64>,
    ended_at: &mut Option<f64>,
    total_damage: &mut f64,
    total_damage_taken: &mut f64,
) {
    stats.clear();
    *started_at = None;
    *ended_at = None;
    *total_damage = 0.0;
    *total_damage_taken = 0.0;
    for hit in hits {
        update_combat_totals(
            stats,
            started_at,
            ended_at,
            total_damage,
            total_damage_taken,
            hit,
        );
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AbyssHalf {
    #[default]
    First,
    Second,
}

impl AbyssHalf {
    pub fn label(self) -> &'static str {
        match self {
            Self::First => "上行线",
            Self::Second => "下行线",
        }
    }
}

#[derive(Clone, Debug)]
pub enum AbyssEvent {
    RestartDetected {
        timestamp: f64,
    },
    Stage {
        timestamp: f64,
        #[allow(dead_code)]
        cycle: Option<u32>,
        floor: Option<u32>,
        half: AbyssHalf,
        allow_late_backfill: bool,
    },
    Success {
        timestamp: f64,
    },
    Exit {
        timestamp: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TimeStopEvent {
    UltraAnimation {
        timestamp: f64,
        char_id: u32,
        ability_id: String,
        duration_seconds: f64,
    },
    ExtraStart {
        timestamp: f64,
        reason: String,
    },
    ExtraEnd {
        timestamp: f64,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TimeStopInterval {
    start: f64,
    end: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TimeStopTracker {
    intervals: Vec<TimeStopInterval>,
    active_extra_starts: HashMap<String, f64>,
    event_count: u64,
}

impl TimeStopTracker {
    fn apply_event(&mut self, event: &TimeStopEvent) {
        self.event_count = self.event_count.saturating_add(1);
        match event {
            TimeStopEvent::UltraAnimation {
                timestamp,
                duration_seconds,
                ..
            } => self.push_interval(*timestamp, *timestamp + duration_seconds),
            TimeStopEvent::ExtraStart { timestamp, reason } => {
                self.active_extra_starts.insert(reason.clone(), *timestamp);
            }
            TimeStopEvent::ExtraEnd { timestamp, reason } => {
                let Some(start) = self.active_extra_starts.remove(reason) else {
                    return;
                };
                self.push_interval(start, *timestamp);
            }
        }
    }

    fn push_interval(&mut self, start: f64, end: f64) {
        if !start.is_finite() || !end.is_finite() || end <= start {
            return;
        }
        self.intervals.push(TimeStopInterval { start, end });
    }

    fn frozen_between(&self, start: f64, end: f64) -> f64 {
        self.intervals_between(start, end)
            .into_iter()
            .map(|interval| interval.end - interval.start)
            .sum()
    }

    fn intervals_between(&self, start: f64, end: f64) -> Vec<TimeStopInterval> {
        if !start.is_finite() || !end.is_finite() || end <= start {
            return Vec::new();
        }
        let mut intervals = self
            .intervals
            .iter()
            .copied()
            .chain(
                self.active_extra_starts
                    .values()
                    .copied()
                    .map(|active_start| TimeStopInterval {
                        start: active_start,
                        end,
                    }),
            )
            .filter_map(|interval| {
                let clipped_start = interval.start.max(start);
                let clipped_end = interval.end.min(end);
                (clipped_end > clipped_start).then_some(TimeStopInterval {
                    start: clipped_start,
                    end: clipped_end,
                })
            })
            .collect::<Vec<_>>();
        intervals.sort_by(|left, right| left.start.total_cmp(&right.start));

        let mut merged_intervals = Vec::new();
        let mut merged: Option<TimeStopInterval> = None;
        for interval in intervals {
            match merged {
                Some(mut current) if interval.start <= current.end => {
                    current.end = current.end.max(interval.end);
                    merged = Some(current);
                }
                Some(current) => {
                    merged_intervals.push(current);
                    merged = Some(interval);
                }
                None => merged = Some(interval),
            }
        }
        if let Some(current) = merged {
            merged_intervals.push(current);
        }
        merged_intervals
    }
}

#[derive(Clone, Debug, Default)]
pub struct PartyCombatState {
    pub hits: VecDeque<Hit>,
    pub hits_generation: u64,
    pub stats: HashMap<u32, CharacterStats>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub total_damage: f64,
    pub total_damage_taken: f64,
    time_stop: TimeStopTracker,
}

impl PartyCombatState {
    pub fn push_hit(&mut self, hit: Hit) {
        update_combat_totals(
            &mut self.stats,
            &mut self.started_at,
            &mut self.ended_at,
            &mut self.total_damage,
            &mut self.total_damage_taken,
            &hit,
        );
        self.hits.push_back(hit);
        self.hits_generation = self.hits_generation.wrapping_add(1);
        if self.hits.len() > MAX_COMBAT_HITS {
            while self.hits.len() > COMBAT_HITS_RETAIN {
                self.hits.pop_front();
            }
            rebuild_combat_totals(
                &self.hits,
                &mut self.stats,
                &mut self.started_at,
                &mut self.ended_at,
                &mut self.total_damage,
                &mut self.total_damage_taken,
            );
        }
    }

    pub fn apply_follow_up(&mut self, follow_up: &HitFollowUp) -> bool {
        let updated = apply_follow_up_to_hits(&mut self.hits, follow_up);
        if updated {
            self.hits_generation = self.hits_generation.wrapping_add(1);
            rebuild_combat_totals(
                &self.hits,
                &mut self.stats,
                &mut self.started_at,
                &mut self.ended_at,
                &mut self.total_damage,
                &mut self.total_damage_taken,
            );
        }
        updated
    }

    pub fn apply_damage_correction(&mut self, correction: &HitDamageCorrection) -> bool {
        let updated = apply_damage_correction_to_hits(&mut self.hits, correction);
        if updated {
            self.hits_generation = self.hits_generation.wrapping_add(1);
            rebuild_combat_totals(
                &self.hits,
                &mut self.stats,
                &mut self.started_at,
                &mut self.ended_at,
                &mut self.total_damage,
                &mut self.total_damage_taken,
            );
        }
        updated
    }

    pub fn duration_with_time_stop(&self, subtract_time_stop: bool) -> f64 {
        match (self.started_at, self.ended_at) {
            (Some(start), Some(end)) => {
                let raw = end - start;
                if subtract_time_stop {
                    (raw - self.time_stop.frozen_between(start, end)).max(0.001)
                } else {
                    raw.max(0.001)
                }
            }
            _ => 0.0,
        }
    }

    pub fn dps_with_time_stop(&self, subtract_time_stop: bool) -> f64 {
        self.total_damage / self.duration_with_time_stop(subtract_time_stop).max(1.0)
    }

    pub fn character_duration_with_time_stop(
        &self,
        row: &CharacterStats,
        subtract_time_stop: bool,
    ) -> f64 {
        character_duration_after_time_stop(row, &self.time_stop, subtract_time_stop)
    }

    pub fn character_dps_with_time_stop(
        &self,
        row: &CharacterStats,
        subtract_time_stop: bool,
    ) -> f64 {
        row.damage
            / self
                .character_duration_with_time_stop(row, subtract_time_stop)
                .max(1.0)
    }

    pub fn apply_time_stop_event(&mut self, event: &TimeStopEvent) {
        self.time_stop.apply_event(event);
    }

    #[allow(dead_code)]
    pub fn time_stop_intervals_between(
        &self,
        start: f64,
        end: f64,
    ) -> Vec<TimelineTimeStopInterval> {
        relative_time_stop_intervals(&self.time_stop, start, end)
    }

    pub fn timeline(&self, bucket_seconds: f64, subtract_time_stop: bool) -> TimelineSeries {
        summarize_timeline_with_time_stop(
            &self.hits,
            &self.time_stop,
            Vec::new(),
            bucket_seconds,
            subtract_time_stop,
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct AbyssRunState {
    pub floor: Option<u32>,
    pub active_half: Option<AbyssHalf>,
    pub pending_restart_at: Option<f64>,
    pub pending_restart_half: Option<AbyssHalf>,
    pub last_half_switch_at: Option<f64>,
    pub last_half_switch_from: Option<AbyssHalf>,
    pub first_half_at: Option<f64>,
    pub second_half_at: Option<f64>,
    pub first_half: PartyCombatState,
    pub second_half: PartyCombatState,
    pub success_at: Option<f64>,
    pub exited_at: Option<f64>,
    pub event_count: u64,
}

impl AbyssRunState {
    pub fn is_active(&self) -> bool {
        self.floor.is_some()
            || !self.first_half.hits.is_empty()
            || !self.second_half.hits.is_empty()
            || self.success_at.is_some()
    }

    pub fn half(&self, half: AbyssHalf) -> &PartyCombatState {
        match half {
            AbyssHalf::First => &self.first_half,
            AbyssHalf::Second => &self.second_half,
        }
    }

    fn half_mut(&mut self, half: AbyssHalf) -> &mut PartyCombatState {
        match half {
            AbyssHalf::First => &mut self.first_half,
            AbyssHalf::Second => &mut self.second_half,
        }
    }

    fn clear_restarted_half(&mut self, half: AbyssHalf, timestamp: f64) {
        *self.half_mut(half) = PartyCombatState::default();
        self.success_at = None;
        self.exited_at = None;
        match half {
            AbyssHalf::First => self.first_half_at = Some(timestamp),
            AbyssHalf::Second => self.second_half_at = Some(timestamp),
        }
    }

    fn clear_restarted_floor(&mut self) {
        self.first_half = PartyCombatState::default();
        self.second_half = PartyCombatState::default();
        self.first_half_at = None;
        self.second_half_at = None;
        self.success_at = None;
        self.exited_at = None;
    }

    pub fn apply_event(&mut self, event: AbyssEvent) {
        self.event_count = self.event_count.saturating_add(1);
        match event {
            AbyssEvent::RestartDetected { timestamp } => {
                if let Some(half) = self.active_half {
                    self.clear_restarted_half(half, timestamp);
                    self.pending_restart_at = Some(timestamp);
                    self.pending_restart_half = Some(half);
                } else {
                    self.pending_restart_at = Some(timestamp);
                    self.pending_restart_half = None;
                }
                self.last_half_switch_at = None;
                self.last_half_switch_from = None;
            }
            AbyssEvent::Stage {
                timestamp,
                cycle: _,
                floor,
                half,
                allow_late_backfill: _,
            } => {
                if floor.is_some() {
                    self.floor = floor;
                }
                if self.active_half.is_some_and(|active| active != half) {
                    self.last_half_switch_at = Some(timestamp);
                    self.last_half_switch_from = self.active_half;
                }
                if let Some(restart_at) = self.pending_restart_at.take() {
                    let restarted_half = self.pending_restart_half.take();
                    if restarted_half.is_some_and(|previous_half| {
                        previous_half != half
                            && timestamp >= restart_at
                            && timestamp - restart_at <= ABYSS_RESTART_STAGE_WINDOW_SECONDS
                    }) {
                        self.clear_restarted_floor();
                    } else if restarted_half.is_none() {
                        self.clear_restarted_half(half, restart_at);
                    }
                }
                self.active_half = Some(half);
                match half {
                    AbyssHalf::First => {
                        self.first_half_at = Some(
                            self.first_half_at
                                .map_or(timestamp, |value| value.min(timestamp)),
                        );
                    }
                    AbyssHalf::Second => {
                        self.second_half_at = Some(
                            self.second_half_at
                                .map_or(timestamp, |value| value.min(timestamp)),
                        );
                    }
                }
            }
            AbyssEvent::Success { timestamp } => self.success_at = Some(timestamp),
            AbyssEvent::Exit { timestamp } => {
                self.exited_at = Some(timestamp);
                self.active_half = None;
                self.pending_restart_at = None;
                self.pending_restart_half = None;
                self.last_half_switch_at = None;
                self.last_half_switch_from = None;
            }
        }
    }

    pub fn push_hit(&mut self, hit: Hit) {
        if let Some(half) = self.active_half {
            self.half_mut(half).push_hit(hit);
        }
    }

    pub fn apply_time_stop_event(&mut self, event: &TimeStopEvent) {
        match event {
            TimeStopEvent::UltraAnimation { .. } | TimeStopEvent::ExtraStart { .. } => {
                if let Some(half) = self.active_half {
                    self.half_mut(half).apply_time_stop_event(event);
                }
            }
            TimeStopEvent::ExtraEnd { .. } => {
                self.first_half.apply_time_stop_event(event);
                self.second_half.apply_time_stop_event(event);
            }
        }
    }

    pub fn timeline_markers_for_half(
        &self,
        half: AbyssHalf,
        start: f64,
        end: f64,
    ) -> Vec<TimelineMarker> {
        let mut markers = Vec::new();
        let (timestamp, label) = match half {
            AbyssHalf::First => (self.first_half_at, "上行线"),
            AbyssHalf::Second => (self.second_half_at, "下行线"),
        };
        push_timeline_marker(
            &mut markers,
            timestamp,
            start,
            end,
            label,
            TimelineMarkerKind::HalfStart,
        );
        push_timeline_marker(
            &mut markers,
            self.success_at,
            start,
            end,
            "通关",
            TimelineMarkerKind::Clear,
        );
        push_timeline_marker(
            &mut markers,
            self.exited_at,
            start,
            end,
            "离开",
            TimelineMarkerKind::Exit,
        );
        sort_timeline_markers(&mut markers);
        markers
    }

    fn timeline_markers_between(&self, start: f64, end: f64) -> Vec<TimelineMarker> {
        let mut markers = Vec::new();
        push_timeline_marker(
            &mut markers,
            self.first_half_at,
            start,
            end,
            "上行线",
            TimelineMarkerKind::HalfStart,
        );
        push_timeline_marker(
            &mut markers,
            self.second_half_at,
            start,
            end,
            "下行线",
            TimelineMarkerKind::HalfStart,
        );
        push_timeline_marker(
            &mut markers,
            self.success_at,
            start,
            end,
            "通关",
            TimelineMarkerKind::Clear,
        );
        push_timeline_marker(
            &mut markers,
            self.exited_at,
            start,
            end,
            "离开",
            TimelineMarkerKind::Exit,
        );
        sort_timeline_markers(&mut markers);
        markers
    }
}

#[derive(Default)]
pub struct CombatState {
    pub hits: VecDeque<Hit>,
    pub hits_generation: u64,
    pub packets: VecDeque<PacketDebug>,
    pub stats: HashMap<u32, CharacterStats>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub total_damage: f64,
    pub total_damage_taken: f64,
    pub abyss: AbyssRunState,
    pub damage_correction_count: u64,
    time_stop: TimeStopTracker,
}

impl CombatState {
    pub fn push_hit(&mut self, hit: Hit) {
        self.abyss.push_hit(hit.clone());
        update_combat_totals(
            &mut self.stats,
            &mut self.started_at,
            &mut self.ended_at,
            &mut self.total_damage,
            &mut self.total_damage_taken,
            &hit,
        );
        self.hits.push_back(hit);
        self.hits_generation = self.hits_generation.wrapping_add(1);
        if self.hits.len() > MAX_COMBAT_HITS {
            while self.hits.len() > COMBAT_HITS_RETAIN {
                self.hits.pop_front();
            }
            rebuild_combat_totals(
                &self.hits,
                &mut self.stats,
                &mut self.started_at,
                &mut self.ended_at,
                &mut self.total_damage,
                &mut self.total_damage_taken,
            );
        }
    }

    pub fn apply_follow_up(&mut self, follow_up: HitFollowUp) {
        let updated = apply_follow_up_to_hits(&mut self.hits, &follow_up);
        if updated {
            self.hits_generation = self.hits_generation.wrapping_add(1);
            rebuild_combat_totals(
                &self.hits,
                &mut self.stats,
                &mut self.started_at,
                &mut self.ended_at,
                &mut self.total_damage,
                &mut self.total_damage_taken,
            );
        }
        self.abyss.first_half.apply_follow_up(&follow_up);
        self.abyss.second_half.apply_follow_up(&follow_up);
    }

    pub fn apply_damage_correction(&mut self, correction: HitDamageCorrection) {
        let updated = apply_damage_correction_to_hits(&mut self.hits, &correction);
        if updated {
            self.damage_correction_count = self.damage_correction_count.saturating_add(1);
            self.hits_generation = self.hits_generation.wrapping_add(1);
            rebuild_combat_totals(
                &self.hits,
                &mut self.stats,
                &mut self.started_at,
                &mut self.ended_at,
                &mut self.total_damage,
                &mut self.total_damage_taken,
            );
        }
        self.abyss.first_half.apply_damage_correction(&correction);
        self.abyss.second_half.apply_damage_correction(&correction);
    }

    pub fn push_packet(&mut self, packet: PacketDebug) {
        self.packets.push_back(packet);
        while self.packets.len() > 10_000 {
            self.packets.pop_front();
        }
    }

    pub fn duration_with_time_stop(&self, subtract_time_stop: bool) -> f64 {
        match (self.started_at, self.ended_at) {
            (Some(start), Some(end)) => {
                let raw = end - start;
                if subtract_time_stop {
                    (raw - self.time_stop.frozen_between(start, end)).max(0.001)
                } else {
                    raw.max(0.001)
                }
            }
            _ => 0.0,
        }
    }

    pub fn dps_with_time_stop(&self, subtract_time_stop: bool) -> f64 {
        self.total_damage / self.duration_with_time_stop(subtract_time_stop).max(1.0)
    }

    pub fn character_duration_with_time_stop(
        &self,
        row: &CharacterStats,
        subtract_time_stop: bool,
    ) -> f64 {
        character_duration_after_time_stop(row, &self.time_stop, subtract_time_stop)
    }

    pub fn character_dps_with_time_stop(
        &self,
        row: &CharacterStats,
        subtract_time_stop: bool,
    ) -> f64 {
        row.damage
            / self
                .character_duration_with_time_stop(row, subtract_time_stop)
                .max(1.0)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn apply_abyss_event(&mut self, event: AbyssEvent) {
        let late_detected_half = match &event {
            AbyssEvent::Stage {
                half,
                allow_late_backfill,
                ..
            } if *allow_late_backfill
                && self.abyss.active_half.is_none()
                && self.abyss.first_half.hits.is_empty()
                && self.abyss.second_half.hits.is_empty()
                && !self.hits.is_empty() =>
            {
                Some(*half)
            }
            _ => None,
        };
        self.abyss.apply_event(event);
        if let Some(half) = late_detected_half {
            self.backfill_abyss_half_from_global(half);
        }
    }

    pub fn apply_time_stop_event(&mut self, event: TimeStopEvent) {
        self.time_stop.apply_event(&event);
        self.abyss.apply_time_stop_event(&event);
    }

    #[allow(dead_code)]
    pub fn time_stop_intervals_between(
        &self,
        start: f64,
        end: f64,
    ) -> Vec<TimelineTimeStopInterval> {
        relative_time_stop_intervals(&self.time_stop, start, end)
    }

    pub fn timeline(&self, bucket_seconds: f64, subtract_time_stop: bool) -> TimelineSeries {
        let mut series = summarize_timeline_with_time_stop(
            &self.hits,
            &self.time_stop,
            Vec::new(),
            bucket_seconds,
            subtract_time_stop,
        );
        if let (Some(start), Some(end)) = (series.start_timestamp, series.end_timestamp) {
            series.markers = self.abyss.timeline_markers_between(start, end);
        }
        series
    }

    pub fn skill_breakdown(&self, char_filter: Option<u32>) -> SkillBreakdown {
        summarize_skill_breakdown(&self.hits, char_filter)
    }

    pub fn capture_quality_summary(&self, source: CaptureQualitySource) -> CaptureQualitySummary {
        let directions = summarize_hit_directions(&self.hits);
        let skills = self.skill_breakdown(None);
        CaptureQualitySummary {
            source,
            packet_count: self.packets.len(),
            packets_with_hits: self
                .packets
                .iter()
                .filter(|packet| packet.parsed_hits > 0)
                .count(),
            hit_count: self.hits.len(),
            outgoing_hits: directions.outgoing_hits,
            outgoing_damage: directions.outgoing_damage,
            unknown_direction_hits: directions.unknown_hits,
            unknown_direction_damage: directions.unknown_damage,
            incoming_hits: directions.incoming_hits,
            incoming_damage: directions.incoming_damage,
            unknown_character_count: skills.unknown.unknown_character_count,
            unknown_character_hits: skills.unknown.unknown_character_hits,
            unmapped_skill_rows: skills.unknown.unmapped_skill_rows,
            unmapped_skill_hits: skills.unknown.unmapped_skill_hits,
            unmapped_gameplay_effect_count: skills.unknown.unmapped_gameplay_effects.len(),
            time_stop_event_count: self.time_stop.event_count,
            time_stop_interval_count: self
                .time_stop
                .intervals_between(
                    self.started_at.unwrap_or_default(),
                    self.ended_at.unwrap_or_default(),
                )
                .len(),
            abyss_event_count: self.abyss.event_count,
            server_damage_corrections: self.damage_correction_count,
        }
    }

    pub fn session_summary(
        &self,
        source: CaptureQualitySource,
        dps_time_mode: impl Into<String>,
        subtract_time_stop: bool,
    ) -> Option<CombatSessionSummary> {
        if self.hits.is_empty() && self.stats.is_empty() && !self.abyss.is_active() {
            return None;
        }
        let duration = self.duration_with_time_stop(subtract_time_stop);
        let skills = summarize_session_skills(self.skill_breakdown(None).rows);
        Some(CombatSessionSummary {
            duration_seconds: duration,
            dps_time_mode: dps_time_mode.into(),
            total_damage: self.total_damage,
            total_dps: self.dps_with_time_stop(subtract_time_stop),
            total_damage_taken: self.total_damage_taken,
            total_hits: self
                .hits
                .iter()
                .filter(|hit| hit.direction != "incoming")
                .count() as u64,
            characters: summarize_session_characters(
                self.stats.values(),
                self.total_damage,
                |row| self.character_dps_with_time_stop(row, subtract_time_stop),
            ),
            skills,
            abyss: summarize_session_abyss(&self.abyss, subtract_time_stop),
            quality: self.capture_quality_summary(source),
        })
    }

    fn backfill_abyss_half_from_global(&mut self, half: AbyssHalf) {
        let party = self.abyss.half_mut(half);
        if !party.hits.is_empty() {
            return;
        }
        for hit in self.hits.iter().cloned() {
            party.push_hit(hit);
        }
        party.time_stop = self.time_stop.clone();
    }
}

fn summarize_session_abyss(
    abyss: &AbyssRunState,
    subtract_time_stop: bool,
) -> CombatSessionAbyssSummary {
    CombatSessionAbyssSummary {
        detected: abyss.is_active(),
        floor: abyss.floor,
        active_half: abyss.active_half.map(|half| half.label().to_owned()),
        success: abyss.success_at.is_some(),
        first_half: summarize_session_abyss_half(
            AbyssHalf::First,
            &abyss.first_half,
            subtract_time_stop,
        ),
        second_half: summarize_session_abyss_half(
            AbyssHalf::Second,
            &abyss.second_half,
            subtract_time_stop,
        ),
    }
}

fn summarize_session_abyss_half(
    half: AbyssHalf,
    party: &PartyCombatState,
    subtract_time_stop: bool,
) -> Option<CombatSessionAbyssHalfSummary> {
    if party.hits.is_empty() && party.stats.is_empty() {
        return None;
    }
    Some(CombatSessionAbyssHalfSummary {
        half: half.label().to_owned(),
        duration_seconds: party.duration_with_time_stop(subtract_time_stop),
        total_damage: party.total_damage,
        total_dps: party.dps_with_time_stop(subtract_time_stop),
        characters: summarize_session_characters(party.stats.values(), party.total_damage, |row| {
            party.character_dps_with_time_stop(row, subtract_time_stop)
        }),
        skills: summarize_session_skills(summarize_skill_breakdown(&party.hits, None).rows),
    })
}

fn summarize_session_characters<'a>(
    rows: impl IntoIterator<Item = &'a CharacterStats>,
    total_damage: f64,
    dps_for_row: impl Fn(&CharacterStats) -> f64,
) -> Vec<CombatSessionCharacterSummary> {
    let mut rows = rows
        .into_iter()
        .filter(|row| row.damage > 0.0 || row.damage_taken > 0.0 || row.hits > 0)
        .map(|row| CombatSessionCharacterSummary {
            char_id: row.char_id,
            name: row.name.clone(),
            hits: row.hits,
            damage: row.damage,
            dps: dps_for_row(row),
            damage_share_percent: if total_damage > 0.0 {
                row.damage / total_damage * 100.0
            } else {
                0.0
            },
            hits_taken: row.hits_taken,
            damage_taken: row.damage_taken,
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .damage
            .total_cmp(&left.damage)
            .then_with(|| left.char_id.cmp(&right.char_id))
    });
    rows
}

fn summarize_session_skills(rows: Vec<SkillBreakdownRow>) -> Vec<CombatSessionSkillSummary> {
    let total_damage = rows.iter().map(|row| row.damage).sum::<f64>();
    rows.into_iter()
        .map(|row| CombatSessionSkillSummary {
            char_id: row.char_id,
            char_name: row.char_name,
            name: row.name,
            category: row.category,
            hits: row.hits,
            damage: row.damage,
            damage_share_percent: if total_damage > 0.0 {
                row.damage / total_damage * 100.0
            } else {
                0.0
            },
            is_follow_up: row.is_follow_up,
        })
        .collect()
}

fn character_duration_after_time_stop(
    row: &CharacterStats,
    time_stop: &TimeStopTracker,
    subtract_time_stop: bool,
) -> f64 {
    let raw = row.duration();
    if raw <= 0.0 {
        return 0.0;
    }
    if !subtract_time_stop {
        return raw;
    }
    (raw - time_stop.frozen_between(row.first_hit, row.last_hit)).max(0.001)
}

fn summarize_timeline_with_time_stop<'a>(
    hits: impl IntoIterator<Item = &'a Hit>,
    time_stop: &TimeStopTracker,
    markers: Vec<TimelineMarker>,
    bucket_seconds: f64,
    _subtract_time_stop: bool,
) -> TimelineSeries {
    let bucket_seconds = if bucket_seconds.is_finite() && bucket_seconds > 0.0 {
        bucket_seconds
    } else {
        1.0
    };
    let hits = hits
        .into_iter()
        .filter(|hit| hit.direction != "incoming" && hit.timestamp.is_finite())
        .collect::<Vec<_>>();
    let Some(start) = hits
        .iter()
        .map(|hit| hit.timestamp)
        .min_by(|left, right| left.total_cmp(right))
    else {
        return TimelineSeries {
            bucket_seconds,
            markers,
            ..Default::default()
        };
    };
    let end = hits
        .iter()
        .map(|hit| hit.timestamp)
        .max_by(|left, right| left.total_cmp(right))
        .unwrap_or(start);
    let bucket_count = ((end - start).max(0.0) / bucket_seconds).floor() as usize + 1;
    let mut buckets = (0..bucket_count)
        .map(|index| TimelineBucket {
            start_offset: index as f64 * bucket_seconds,
            end_offset: (index + 1) as f64 * bucket_seconds,
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let mut role_buckets = vec![HashMap::<u32, (String, f64)>::new(); bucket_count];

    for hit in hits {
        let damage = hit.total_damage();
        if !damage.is_finite() {
            continue;
        }
        let bucket_index = (((hit.timestamp - start).max(0.0) / bucket_seconds).floor() as usize)
            .min(bucket_count - 1);
        let bucket = &mut buckets[bucket_index];
        bucket.damage += damage;
        bucket.hits += 1;
        let role = role_buckets[bucket_index]
            .entry(hit.char_id)
            .or_insert_with(|| (hit.char_name.clone(), 0.0));
        role.0.clone_from(&hit.char_name);
        role.1 += damage;
    }

    let mut total_damage = 0.0;
    for (index, bucket) in buckets.iter_mut().enumerate() {
        total_damage += bucket.damage;
        bucket.cumulative_damage = total_damage;
        // Timeline buckets stay on real wall-clock seconds. Time-stop periods
        // are drawn as bands; subtracting them inside a fixed 1s bucket can
        // shrink the divisor to almost zero and produce unusable peak spikes.
        let duration = bucket_seconds.max(0.001);
        bucket.dps = bucket.damage / duration;
        let mut roles = role_buckets[index]
            .drain()
            .map(|(char_id, (char_name, damage))| TimelineRoleBucket {
                char_id,
                char_name,
                damage,
                dps: damage / duration,
            })
            .collect::<Vec<_>>();
        roles.sort_by(|left, right| {
            right
                .damage
                .total_cmp(&left.damage)
                .then_with(|| left.char_name.cmp(&right.char_name))
                .then_with(|| left.char_id.cmp(&right.char_id))
        });
        bucket.role_damage = roles;
    }

    TimelineSeries {
        bucket_seconds,
        start_timestamp: Some(start),
        end_timestamp: Some(end),
        total_damage,
        buckets,
        time_stop_intervals: relative_time_stop_intervals(time_stop, start, end),
        markers,
    }
}

fn relative_time_stop_intervals(
    time_stop: &TimeStopTracker,
    start: f64,
    end: f64,
) -> Vec<TimelineTimeStopInterval> {
    time_stop
        .intervals_between(start, end)
        .into_iter()
        .map(|interval| TimelineTimeStopInterval {
            start_offset: interval.start - start,
            end_offset: interval.end - start,
        })
        .collect()
}

fn push_timeline_marker(
    markers: &mut Vec<TimelineMarker>,
    timestamp: Option<f64>,
    start: f64,
    end: f64,
    label: &str,
    kind: TimelineMarkerKind,
) {
    let Some(timestamp) = timestamp else {
        return;
    };
    if !timestamp.is_finite() {
        return;
    }
    let timestamp = timestamp.clamp(start, end);
    markers.push(TimelineMarker {
        offset: timestamp - start,
        label: label.to_owned(),
        kind,
    });
}

fn sort_timeline_markers(markers: &mut [TimelineMarker]) {
    markers.sort_by(|left, right| {
        left.offset
            .total_cmp(&right.offset)
            .then_with(|| left.label.cmp(&right.label))
    });
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Hit(Box<Hit>),
    HitFollowUp(HitFollowUp),
    HitDamageCorrection(HitDamageCorrection),
    Packet(Box<PacketDebug>),
    Abyss(AbyssEvent),
    TimeStop(TimeStopEvent),
    Status(String),
    Warning(String),
    Error(String),
    CaptureStopped,
}

fn apply_follow_up_to_hits(hits: &mut VecDeque<Hit>, follow_up: &HitFollowUp) -> bool {
    let Some(hit) = hits
        .iter_mut()
        .rev()
        .find(|hit| hit_matches_follow_up_source(hit, follow_up))
    else {
        return false;
    };
    hit.follow_up_damage += follow_up.damage;
    hit.follow_up_timestamp = Some(follow_up.timestamp);
    hit.follow_up_damage_name = follow_up.damage_name.clone();
    hit.follow_up_attack_type = follow_up.attack_type.clone();
    hit.follow_up_damage_attribute = follow_up.damage_attribute.clone();
    hit.target_hp_after = follow_up.target_hp_after;
    hit.target_hp_percent = follow_up.target_hp_percent;
    true
}

fn apply_damage_correction_to_hits(
    hits: &mut VecDeque<Hit>,
    correction: &HitDamageCorrection,
) -> bool {
    let Some(hit) = hits
        .iter_mut()
        .rev()
        .find(|hit| hit_matches_damage_correction_source(hit, correction))
    else {
        return false;
    };
    hit.damage = correction.damage;
    hit.target_hp_before = correction.target_hp_before;
    hit.target_hp_after = correction.target_hp_after;
    hit.target_hp_percent = correction.target_hp_percent;
    true
}

fn hit_matches_follow_up_source(hit: &Hit, follow_up: &HitFollowUp) -> bool {
    hit.char_id == follow_up.source_char_id
        && (hit.timestamp - follow_up.source_timestamp).abs() <= 0.001
        && (hit.damage - follow_up.source_damage).abs() <= 0.5
        && (hit.target_hp_before - follow_up.source_target_hp_before).abs() <= 0.5
        && (hit.target_hp_after - follow_up.source_target_hp_after).abs() <= 0.5
        && (hit.target_max_hp - follow_up.source_target_max_hp).abs() <= 0.5
        && hit.gameplay_effect_index == follow_up.source_gameplay_effect_index
}

fn hit_matches_damage_correction_source(hit: &Hit, correction: &HitDamageCorrection) -> bool {
    hit.char_id == correction.source_char_id
        && (hit.timestamp - correction.source_timestamp).abs() <= 0.001
        && (hit.damage - correction.source_damage).abs() <= 0.5
        && (hit.target_hp_before - correction.source_target_hp_before).abs() <= 0.5
        && (hit.target_hp_after - correction.source_target_hp_after).abs() <= 0.5
        && (hit.target_max_hp - correction.source_target_max_hp).abs() <= 0.5
        && hit.gameplay_effect_index == correction.source_gameplay_effect_index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_dps_export_is_compact_and_roundtrips() {
        let export = TeamDpsExport {
            version: TEAM_DPS_EXPORT_VERSION,
            single: None,
            upper: Some(TeamDps {
                dps: 31535.0,
                members: vec![TeamDpsMember {
                    id: 1010,
                    dps: 23700.0,
                    name: "娜娜莉".to_owned(),
                }],
            }),
            lower: None,
        };
        let json = serde_json::to_string(&export).unwrap();
        // Compact (no pretty newlines) and absent teams are omitted to stay small.
        assert!(!json.contains('\n'));
        assert!(!json.contains("single"));
        assert!(!json.contains("lower"));

        let parsed: TeamDpsExport = serde_json::from_str(&json).unwrap();
        assert!(parsed.single.is_none());
        assert_eq!(parsed.upper.as_ref().unwrap().members[0].id, 1010);
        assert_eq!(parsed.upper.as_ref().unwrap().members.len(), 1);
    }

    #[test]
    fn team_dps_export_version_defaults_when_missing() {
        let parsed: TeamDpsExport = serde_json::from_str(r#"{"single":{"dps":100.0}}"#).unwrap();
        assert_eq!(parsed.version, TEAM_DPS_EXPORT_VERSION);
        assert_eq!(parsed.single.unwrap().dps, 100.0);
    }

    fn test_hit(timestamp: f64, char_id: u32, direction: &str, damage: f64) -> Hit {
        Hit {
            timestamp,
            char_id,
            char_name: format!("角色{char_id}"),
            char_known: true,
            damage,
            byte_offset: 0,
            bit_shift: 0,
            char_source: "test".to_owned(),
            direction: direction.to_owned(),
            target_hp_before: 0.0,
            target_hp_after: 0.0,
            target_max_hp: 0.0,
            target_hp_percent: 0.0,
            target_id: None,
            target_name: None,
            target_context: Vec::new(),
            gameplay_effect_index: None,
            gameplay_effect_name: None,
            ability_name: None,
            damage_name: None,
            attack_type: None,
            damage_attribute: None,
            follow_up_damage: 0.0,
            follow_up_timestamp: None,
            follow_up_damage_name: None,
            follow_up_attack_type: None,
            follow_up_damage_attribute: None,
        }
    }

    fn assert_totals_match_hits(
        hits: &VecDeque<Hit>,
        stats: &HashMap<u32, CharacterStats>,
        total_damage: f64,
        total_damage_taken: f64,
        duration: f64,
    ) {
        assert!(hits.len() <= MAX_COMBAT_HITS);
        assert!(hits.len() >= COMBAT_HITS_RETAIN);

        let expected_damage: f64 = hits
            .iter()
            .filter(|hit| hit.direction != "incoming")
            .map(Hit::total_damage)
            .sum();
        let expected_damage_taken: f64 = hits
            .iter()
            .filter(|hit| hit.direction == "incoming")
            .map(Hit::total_damage)
            .sum();
        assert_eq!(total_damage, expected_damage);
        assert_eq!(total_damage_taken, expected_damage_taken);

        for hit in hits {
            assert!(stats.contains_key(&hit.char_id));
        }
        for (&char_id, row) in stats {
            let char_hits: Vec<_> = hits.iter().filter(|hit| hit.char_id == char_id).collect();
            assert_eq!(
                row.hits,
                char_hits
                    .iter()
                    .filter(|hit| hit.direction != "incoming")
                    .count() as u64
            );
            assert_eq!(
                row.damage,
                char_hits
                    .iter()
                    .filter(|hit| hit.direction != "incoming")
                    .map(|hit| hit.total_damage())
                    .sum::<f64>()
            );
            assert_eq!(
                row.hits_taken,
                char_hits
                    .iter()
                    .filter(|hit| hit.direction == "incoming")
                    .count() as u64
            );
            assert_eq!(
                row.damage_taken,
                char_hits
                    .iter()
                    .filter(|hit| hit.direction == "incoming")
                    .map(|hit| hit.total_damage())
                    .sum::<f64>()
            );
        }

        let outgoing_timestamps: Vec<_> = hits
            .iter()
            .filter(|hit| hit.direction != "incoming")
            .map(|hit| hit.timestamp)
            .collect();
        let expected_duration =
            (outgoing_timestamps.last().unwrap() - outgoing_timestamps.first().unwrap()).max(0.001);
        assert_eq!(duration, expected_duration);
        assert!(duration < 100_000.0);
    }

    fn overflowing_hits() -> Vec<Hit> {
        let mut hits = Vec::with_capacity(MAX_COMBAT_HITS + 1);
        hits.push(test_hit(-100_000.0, 99, "outgoing", 1_000_000.0));
        for index in 0..MAX_COMBAT_HITS {
            let direction = if index % 3 == 0 {
                "incoming"
            } else {
                "outgoing"
            };
            hits.push(test_hit(
                index as f64,
                (index % 4 + 1) as u32,
                direction,
                (index % 10 + 1) as f64,
            ));
        }
        hits
    }

    #[test]
    fn unknown_hits_remain_output_and_directions_are_summarized_separately() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(1.0, 1, "outgoing", 100.0));
        state.push_hit(test_hit(2.0, 1, "unknown", 40.0));
        state.push_hit(test_hit(3.0, 1, "incoming", 25.0));

        assert_eq!(state.total_damage, 140.0);
        assert_eq!(state.total_damage_taken, 25.0);
        let summary = summarize_hit_directions(&state.hits);
        assert_eq!(summary.outgoing_damage, 100.0);
        assert_eq!(summary.outgoing_hits, 1);
        assert_eq!(summary.unknown_damage, 40.0);
        assert_eq!(summary.unknown_hits, 1);
        assert_eq!(summary.incoming_damage, 25.0);
        assert_eq!(summary.incoming_hits, 1);
        assert!((summary.unknown_share() - 28.571_428_571).abs() < 1e-6);
    }

    #[test]
    fn timeline_handles_empty_hits() {
        let hits = Vec::<Hit>::new();
        let timeline = summarize_timeline(hits.iter(), 1.0);

        assert_eq!(timeline.bucket_seconds, 1.0);
        assert!(timeline.buckets.is_empty());
        assert_eq!(timeline.total_damage, 0.0);
    }

    #[test]
    fn timeline_buckets_damage_by_second_and_role() {
        let mut first = test_hit(10.0, 1, "outgoing", 100.0);
        first.char_name = "一号".to_owned();
        let mut same_bucket = test_hit(10.9, 2, "unknown", 50.0);
        same_bucket.char_name = "二号".to_owned();
        let mut next_bucket = test_hit(11.0, 1, "outgoing", 200.0);
        next_bucket.char_name = "一号".to_owned();
        let incoming = test_hit(11.2, 3, "incoming", 999.0);
        let hits = Vec::from([first, same_bucket, next_bucket, incoming]);

        let timeline = summarize_timeline(hits.iter(), 1.0);

        assert_eq!(timeline.buckets.len(), 2);
        assert_eq!(timeline.total_damage, 350.0);
        assert_eq!(timeline.buckets[0].damage, 150.0);
        assert_eq!(timeline.buckets[0].hits, 2);
        assert_eq!(timeline.buckets[0].role_damage.len(), 2);
        assert_eq!(timeline.buckets[1].damage, 200.0);
        assert_eq!(timeline.buckets[1].cumulative_damage, 350.0);
    }

    #[test]
    fn timeline_marks_time_stop_without_inflating_bucket_dps() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(0.0, 1, "outgoing", 100.0));
        state.apply_time_stop_event(TimeStopEvent::UltraAnimation {
            timestamp: 0.25,
            char_id: 1,
            ability_id: "GA_Test_UltraSkill".to_owned(),
            duration_seconds: 0.5,
        });
        state.push_hit(test_hit(1.0, 1, "outgoing", 100.0));

        let timeline = state.timeline(1.0, true);

        assert_eq!(timeline.time_stop_intervals.len(), 1);
        assert!((timeline.time_stop_intervals[0].start_offset - 0.25).abs() < 1e-9);
        assert!((timeline.time_stop_intervals[0].end_offset - 0.75).abs() < 1e-9);
        assert!((timeline.buckets[0].dps - 100.0).abs() < 1e-9);
    }

    #[test]
    fn timeline_clamps_abyss_markers_to_chart_edges() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 0.0,
            cycle: Some(1),
            floor: Some(1),
            half: AbyssHalf::First,
            allow_late_backfill: false,
        });
        state.push_hit(test_hit(1.0, 1, "outgoing", 100.0));
        state.push_hit(test_hit(2.0, 1, "outgoing", 100.0));
        state.apply_abyss_event(AbyssEvent::Success { timestamp: 3.0 });
        state.apply_abyss_event(AbyssEvent::Exit { timestamp: 4.0 });

        let timeline = state.timeline(1.0, false);

        assert_eq!(timeline.start_timestamp, Some(1.0));
        assert_eq!(timeline.end_timestamp, Some(2.0));
        assert!(timeline.markers.iter().any(|marker| {
            marker.label == "上行线"
                && marker.kind == TimelineMarkerKind::HalfStart
                && marker.offset == 0.0
        }));
        assert!(timeline.markers.iter().any(|marker| {
            marker.label == "通关"
                && marker.kind == TimelineMarkerKind::Clear
                && (marker.offset - 1.0).abs() < 1e-9
        }));
        assert!(timeline.markers.iter().any(|marker| {
            marker.label == "离开"
                && marker.kind == TimelineMarkerKind::Exit
                && (marker.offset - 1.0).abs() < 1e-9
        }));
    }

    #[test]
    fn abyss_half_timeline_can_include_run_markers() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 0.0,
            cycle: Some(1),
            floor: Some(1),
            half: AbyssHalf::First,
            allow_late_backfill: false,
        });
        state.push_hit(test_hit(1.0, 1, "outgoing", 100.0));
        state.push_hit(test_hit(2.0, 1, "outgoing", 100.0));
        state.apply_abyss_event(AbyssEvent::Success { timestamp: 3.0 });
        state.apply_abyss_event(AbyssEvent::Exit { timestamp: 4.0 });

        let mut timeline = state.abyss.first_half.timeline(1.0, false);
        if let (Some(start), Some(end)) = (timeline.start_timestamp, timeline.end_timestamp) {
            timeline.markers = state
                .abyss
                .timeline_markers_for_half(AbyssHalf::First, start, end);
        }

        assert_eq!(timeline.start_timestamp, Some(1.0));
        assert_eq!(timeline.end_timestamp, Some(2.0));
        assert!(timeline.markers.iter().any(|marker| {
            marker.label == "上行线"
                && marker.kind == TimelineMarkerKind::HalfStart
                && marker.offset == 0.0
        }));
        assert!(timeline.markers.iter().any(|marker| {
            marker.label == "通关"
                && marker.kind == TimelineMarkerKind::Clear
                && (marker.offset - 1.0).abs() < 1e-9
        }));
        assert!(timeline.markers.iter().any(|marker| {
            marker.label == "离开"
                && marker.kind == TimelineMarkerKind::Exit
                && (marker.offset - 1.0).abs() < 1e-9
        }));
    }

    #[test]
    fn skill_breakdown_splits_follow_up_and_unknown_attribution() {
        let mut source = test_hit(1.0, 10, "outgoing", 100.0);
        source.char_name = "主输出".to_owned();
        source.attack_type = Some("普攻".to_owned());
        source.damage_name = Some("普攻一段".to_owned());
        source.ability_name = Some("GA_Test_Melee".to_owned());
        source.follow_up_damage = 25.0;
        source.follow_up_damage_name = Some("覆纹追加".to_owned());
        source.follow_up_attack_type = Some("覆纹".to_owned());

        let mut unknown = test_hit(2.0, 99, "unknown", 50.0);
        unknown.char_known = false;
        unknown.char_name = "未知角色".to_owned();
        unknown.gameplay_effect_index = Some(777);

        let hits = Vec::from([source, unknown]);
        let breakdown = summarize_skill_breakdown(hits.iter(), None);

        assert_eq!(breakdown.total_damage, 175.0);
        assert_eq!(breakdown.total_hits, 3);
        assert_eq!(breakdown.rows.len(), 3);
        assert!(
            breakdown
                .rows
                .iter()
                .any(|row| row.name == "覆纹追加" && row.is_follow_up)
        );
        assert_eq!(breakdown.unknown.unknown_character_count, 1);
        assert_eq!(breakdown.unknown.unknown_direction_hits, 1);
        assert_eq!(breakdown.unknown.unmapped_skill_hits, 1);
        assert_eq!(breakdown.unknown.unmapped_gameplay_effects[0].index, 777);
    }

    #[test]
    fn skill_breakdown_merges_same_ability_across_effects() {
        let mut first = test_hit(1.0, 10, "outgoing", 100.0);
        first.attack_type = Some("Q技能".to_owned());
        first.ability_name = Some("GA_Test_UltraSkill".to_owned());
        first.gameplay_effect_index = Some(101);
        first.gameplay_effect_name = Some("GE_Test_UltraSkill1_Damage".to_owned());

        let mut second = test_hit(2.0, 10, "outgoing", 75.0);
        second.attack_type = Some("Q技能".to_owned());
        second.ability_name = Some("GA_Test_UltraSkill".to_owned());
        second.gameplay_effect_index = Some(102);
        second.gameplay_effect_name = Some("GE_Test_UltraSkill2_Damage".to_owned());

        let hits = Vec::from([first, second]);
        let breakdown = summarize_skill_breakdown(hits.iter(), None);

        assert_eq!(breakdown.rows.len(), 1);
        assert_eq!(breakdown.rows[0].name, "GA_Test_UltraSkill");
        assert_eq!(breakdown.rows[0].hits, 2);
        assert_eq!(breakdown.rows[0].damage, 175.0);
    }

    #[test]
    fn capture_quality_summary_is_redacted() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(1.0, 1, "outgoing", 100.0));
        state.push_packet(PacketDebug {
            timestamp: 1.0,
            source: "192.0.2.1:1111".to_owned(),
            destination: "198.51.100.1:2222".to_owned(),
            direction: "outgoing".to_owned(),
            payload_len: 128,
            declared_ids: vec![1],
            parsed_hits: 1,
            note: "sensitive note".to_owned(),
            payload_preview: "preview text".to_owned(),
            payload_hex: "deadbeef".to_owned(),
            decoded_text: "decoded text".to_owned(),
        });
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 1.0,
            cycle: None,
            floor: Some(1),
            half: AbyssHalf::First,
            allow_late_backfill: false,
        });

        let summary = state.capture_quality_summary(CaptureQualitySource::PcapngReplay);
        let text = summary.redacted_text();

        assert_eq!(summary.packet_count, 1);
        assert_eq!(summary.packets_with_hits, 1);
        assert_eq!(summary.abyss_event_count, 1);
        assert!(text.contains("PCAPNG 回放"));
        assert!(!text.contains("deadbeef"));
        assert!(!text.contains("192.0.2.1"));
        assert!(!text.contains("decoded text"));
    }

    #[test]
    fn combat_session_summary_contains_redacted_aggregates() {
        let mut state = CombatState::default();
        let mut hit = test_hit(1.0, 10, "outgoing", 120.0);
        hit.char_name = "测试角色".to_owned();
        hit.attack_type = Some("普攻".to_owned());
        hit.damage_name = Some("普攻一段".to_owned());
        state.push_hit(hit);

        let summary = state
            .session_summary(CaptureQualitySource::JsonReplay, "扣除时停", true)
            .expect("summary should exist");

        assert_eq!(summary.dps_time_mode, "扣除时停");
        assert_eq!(summary.total_damage, 120.0);
        assert_eq!(summary.total_hits, 1);
        assert_eq!(summary.characters[0].name, "测试角色");
        assert_eq!(summary.skills[0].name, "普攻一段");
        assert_eq!(summary.quality.source, CaptureQualitySource::JsonReplay);
    }

    #[test]
    fn combat_session_summary_keeps_abyss_halves_separate() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 0.0,
            cycle: None,
            floor: Some(1),
            half: AbyssHalf::First,
            allow_late_backfill: false,
        });
        let mut first = test_hit(1.0, 1, "outgoing", 100.0);
        first.char_name = "上半角色".to_owned();
        first.damage_name = Some("上半技能".to_owned());
        state.push_hit(first);
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 5.0,
            cycle: None,
            floor: Some(1),
            half: AbyssHalf::Second,
            allow_late_backfill: false,
        });
        let mut second = test_hit(6.0, 2, "outgoing", 200.0);
        second.char_name = "下半角色".to_owned();
        second.damage_name = Some("下半技能".to_owned());
        state.push_hit(second);

        let summary = state
            .session_summary(CaptureQualitySource::JsonReplay, "扣除时停", true)
            .expect("summary should exist");
        let first_half = summary.abyss.first_half.expect("first half summary");
        let second_half = summary.abyss.second_half.expect("second half summary");

        assert_eq!(first_half.characters[0].name, "上半角色");
        assert_eq!(first_half.skills[0].name, "上半技能");
        assert_eq!(second_half.characters[0].name, "下半角色");
        assert_eq!(second_half.skills[0].name, "下半技能");
    }

    #[test]
    fn follow_up_damage_merges_into_source_hit_totals() {
        let mut state = CombatState::default();
        let mut hit = test_hit(1.0, 7, "outgoing", 1_000.0);
        hit.target_hp_before = 10_000.0;
        hit.target_hp_after = 9_000.0;
        hit.target_max_hp = 10_000.0;
        hit.gameplay_effect_index = Some(42);
        state.push_hit(hit);

        state.apply_follow_up(HitFollowUp {
            source_timestamp: 1.0,
            source_char_id: 7,
            source_damage: 1_000.0,
            source_target_hp_before: 10_000.0,
            source_target_hp_after: 9_000.0,
            source_target_max_hp: 10_000.0,
            source_gameplay_effect_index: Some(42),
            timestamp: 1.2,
            damage: 250.0,
            target_hp_after: 8_750.0,
            target_hp_percent: 87.5,
            damage_name: Some("覆纹追加攻击".to_owned()),
            attack_type: Some("覆纹".to_owned()),
            damage_attribute: Some("灵".to_owned()),
        });

        let merged = state.hits.front().unwrap();
        assert_eq!(merged.damage, 1_000.0);
        assert_eq!(merged.follow_up_damage, 250.0);
        assert_eq!(merged.target_hp_after, 8_750.0);
        assert_eq!(state.total_damage, 1_250.0);
        let stats = state.stats.get(&7).unwrap();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.damage, 1_250.0);
        assert_eq!(state.damage_correction_count, 0);
    }

    #[test]
    fn damage_correction_replaces_source_hit_totals() {
        let mut state = CombatState::default();
        let mut hit = test_hit(1.0, 7, "outgoing", 1_000.0);
        hit.target_hp_before = 10_000.0;
        hit.target_hp_after = 9_000.0;
        hit.target_max_hp = 10_000.0;
        hit.gameplay_effect_index = Some(42);
        state.push_hit(hit);

        state.apply_damage_correction(HitDamageCorrection {
            source_timestamp: 1.0,
            source_char_id: 7,
            source_damage: 1_000.0,
            source_target_hp_before: 10_000.0,
            source_target_hp_after: 9_000.0,
            source_target_max_hp: 10_000.0,
            source_gameplay_effect_index: Some(42),
            damage: 1_250.0,
            target_hp_before: 10_250.0,
            target_hp_after: 9_000.0,
            target_hp_percent: 90.0,
        });

        let corrected = state.hits.front().unwrap();
        assert_eq!(corrected.damage, 1_250.0);
        assert_eq!(corrected.follow_up_damage, 0.0);
        assert_eq!(corrected.target_hp_before, 10_250.0);
        assert_eq!(state.total_damage, 1_250.0);
        let stats = state.stats.get(&7).unwrap();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.damage, 1_250.0);
        assert_eq!(state.damage_correction_count, 1);
    }

    #[test]
    fn abyss_half_labels_are_utf8_chinese() {
        assert_eq!(AbyssHalf::First.label(), "上行线");
        assert_eq!(AbyssHalf::Second.label(), "下行线");
    }

    #[test]
    fn party_combat_totals_follow_trimmed_hits() {
        let mut state = PartyCombatState::default();
        let hits = overflowing_hits();
        let expected_generation = hits.len() as u64;
        for hit in hits {
            state.push_hit(hit);
        }

        assert_eq!(state.hits_generation, expected_generation);
        assert_totals_match_hits(
            &state.hits,
            &state.stats,
            state.total_damage,
            state.total_damage_taken,
            state.duration_with_time_stop(true),
        );
        assert!(!state.stats.contains_key(&99));
    }

    #[test]
    fn combat_totals_follow_trimmed_hits() {
        let mut state = CombatState::default();
        let hits = overflowing_hits();
        let expected_generation = hits.len() as u64;
        for hit in hits {
            state.push_hit(hit);
        }

        assert_eq!(state.hits_generation, expected_generation);
        assert_totals_match_hits(
            &state.hits,
            &state.stats,
            state.total_damage,
            state.total_damage_taken,
            state.duration_with_time_stop(true),
        );
        assert!(!state.stats.contains_key(&99));
    }

    #[test]
    fn combat_duration_subtracts_ultra_animation_time_stop() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(10.0, 1021, "outgoing", 100.0));
        state.apply_time_stop_event(TimeStopEvent::UltraAnimation {
            timestamp: 11.0,
            char_id: 1021,
            ability_id: "GA_Edgar_UltraSkill".to_owned(),
            duration_seconds: 3.0,
        });
        state.push_hit(test_hit(20.0, 1021, "outgoing", 200.0));

        assert!((state.duration_with_time_stop(true) - 7.0).abs() < 1e-9);
        assert!((state.dps_with_time_stop(true) - (300.0 / 7.0)).abs() < 1e-9);
    }

    #[test]
    fn character_duration_subtracts_time_stop() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(10.0, 1021, "outgoing", 100.0));
        state.apply_time_stop_event(TimeStopEvent::UltraAnimation {
            timestamp: 11.0,
            char_id: 1021,
            ability_id: "GA_Edgar_UltraSkill".to_owned(),
            duration_seconds: 3.0,
        });
        state.push_hit(test_hit(20.0, 1021, "outgoing", 200.0));

        let row = state.stats.get(&1021).unwrap();
        assert!((row.duration() - 10.0).abs() < 1e-9);
        assert!((state.character_duration_with_time_stop(row, true) - 7.0).abs() < 1e-9);
        assert!((state.character_dps_with_time_stop(row, true) - (300.0 / 7.0)).abs() < 1e-9);
        assert!((state.duration_with_time_stop(false) - 10.0).abs() < 1e-9);
        assert!((state.dps_with_time_stop(false) - 30.0).abs() < 1e-9);
        assert!((state.character_duration_with_time_stop(row, false) - 10.0).abs() < 1e-9);
        assert!((state.character_dps_with_time_stop(row, false) - 30.0).abs() < 1e-9);
    }

    #[test]
    fn overlapping_animation_and_extra_time_stop_are_unioned() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(0.0, 1052, "outgoing", 100.0));
        state.apply_time_stop_event(TimeStopEvent::UltraAnimation {
            timestamp: 1.0,
            char_id: 1052,
            ability_id: "GA_Jin_UltraSkill".to_owned(),
            duration_seconds: 2.0,
        });
        state.apply_time_stop_event(TimeStopEvent::ExtraStart {
            timestamp: 2.0,
            reason: "Event.Montage.Player.UltraSkill.Jin".to_owned(),
        });
        state.apply_time_stop_event(TimeStopEvent::ExtraEnd {
            timestamp: 5.0,
            reason: "Event.Montage.Player.UltraSkill.Jin".to_owned(),
        });
        state.push_hit(test_hit(10.0, 1052, "outgoing", 100.0));

        assert!((state.duration_with_time_stop(true) - 6.0).abs() < 1e-9);
    }

    #[test]
    fn open_extra_time_stop_is_clipped_to_combat_end() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(0.0, 1052, "outgoing", 100.0));
        state.apply_time_stop_event(TimeStopEvent::ExtraStart {
            timestamp: 4.0,
            reason: "Event.Montage.Player.UltraSkill.Jin".to_owned(),
        });
        state.push_hit(test_hit(10.0, 1052, "outgoing", 100.0));

        assert!((state.duration_with_time_stop(true) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn repeated_extra_time_stop_start_replaces_stale_start() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(0.0, 1052, "outgoing", 100.0));
        state.apply_time_stop_event(TimeStopEvent::ExtraStart {
            timestamp: 1.0,
            reason: "Event.Montage.Player.UltraSkill.Jin".to_owned(),
        });
        state.apply_time_stop_event(TimeStopEvent::ExtraStart {
            timestamp: 10.0,
            reason: "Event.Montage.Player.UltraSkill.Jin".to_owned(),
        });
        state.apply_time_stop_event(TimeStopEvent::ExtraEnd {
            timestamp: 12.0,
            reason: "Event.Montage.Player.UltraSkill.Jin".to_owned(),
        });
        state.push_hit(test_hit(20.0, 1052, "outgoing", 100.0));

        assert!((state.duration_with_time_stop(true) - 18.0).abs() < 1e-9);
    }

    #[test]
    fn abyss_active_half_duration_subtracts_time_stop() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 0.0,
            cycle: Some(1),
            floor: Some(1),
            half: AbyssHalf::First,
            allow_late_backfill: false,
        });
        state.push_hit(test_hit(1.0, 1010, "outgoing", 100.0));
        state.apply_time_stop_event(TimeStopEvent::UltraAnimation {
            timestamp: 2.0,
            char_id: 1010,
            ability_id: "GA_Nanally_UltraSkill".to_owned(),
            duration_seconds: 2.0,
        });
        state.push_hit(test_hit(6.0, 1010, "outgoing", 100.0));

        assert!((state.abyss.first_half.duration_with_time_stop(true) - 3.0).abs() < 1e-9);
        let row = state.abyss.first_half.stats.get(&1010).unwrap();
        assert!(
            (state
                .abyss
                .first_half
                .character_duration_with_time_stop(row, true)
                - 3.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn late_detected_second_half_backfills_existing_global_hits() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(10.0, 1010, "outgoing", 100.0));
        state.apply_time_stop_event(TimeStopEvent::UltraAnimation {
            timestamp: 11.0,
            char_id: 1010,
            ability_id: "GA_Nanally_UltraSkill".to_owned(),
            duration_seconds: 2.0,
        });
        state.push_hit(test_hit(15.0, 1010, "outgoing", 200.0));

        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 20.0,
            cycle: None,
            floor: None,
            half: AbyssHalf::Second,
            allow_late_backfill: true,
        });
        state.apply_abyss_event(AbyssEvent::Success { timestamp: 21.0 });

        assert_eq!(state.total_damage, 300.0);
        assert_eq!(state.abyss.first_half.total_damage, 0.0);
        assert_eq!(state.abyss.second_half.total_damage, 300.0);
        assert_eq!(state.abyss.second_half.hits.len(), 2);
        assert!((state.abyss.second_half.duration_with_time_stop(true) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn first_normal_stage_does_not_backfill_previous_global_hits() {
        let mut state = CombatState::default();
        state.push_hit(test_hit(1.0, 1010, "outgoing", 100.0));

        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 20.0,
            cycle: None,
            floor: None,
            half: AbyssHalf::First,
            allow_late_backfill: false,
        });

        assert_eq!(state.total_damage, 100.0);
        assert_eq!(state.abyss.first_half.total_damage, 0.0);
        assert!(state.abyss.first_half.hits.is_empty());
    }

    fn timeline_from_pattern(pattern: &[bool]) -> TimelineSeries {
        let buckets = pattern
            .iter()
            .enumerate()
            .map(|(index, &active)| TimelineBucket {
                start_offset: index as f64,
                end_offset: (index + 1) as f64,
                damage: if active { 100.0 } else { 0.0 },
                hits: u64::from(active),
                ..Default::default()
            })
            .collect();
        TimelineSeries {
            bucket_seconds: 1.0,
            buckets,
            ..Default::default()
        }
    }

    #[test]
    fn combat_segments_split_on_long_idle_gap() {
        // 3 active buckets, a 6s idle gap (> 5s), then 2 active buckets.
        let series = timeline_from_pattern(&[
            true, true, true, false, false, false, false, false, false, true, true,
        ]);
        let segments = summarize_combat_segments(&series, 5.0);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].start_offset, 0.0);
        assert_eq!(segments[0].end_offset, 3.0);
        assert_eq!(segments[0].total_damage, 300.0);
        assert_eq!(segments[0].hits, 3);
        assert_eq!(segments[0].duration, 3.0);
        assert_eq!(segments[0].dps, 100.0);
        assert_eq!(segments[1].start_offset, 9.0);
        assert_eq!(segments[1].total_damage, 200.0);
    }

    #[test]
    fn combat_segments_keep_short_gaps_together() {
        // A 2s gap (< 5s) does not split the fight.
        let series = timeline_from_pattern(&[true, false, false, true]);
        let segments = summarize_combat_segments(&series, 5.0);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].total_damage, 200.0);
        assert_eq!(segments[0].hits, 2);
    }

    #[test]
    fn combat_segments_empty_series_has_no_segments() {
        assert!(summarize_combat_segments(&TimelineSeries::default(), 5.0).is_empty());
    }
}
