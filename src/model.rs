use std::collections::{HashMap, VecDeque};

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
}

impl TimeStopTracker {
    fn apply_event(&mut self, event: &TimeStopEvent) {
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
        if !start.is_finite() || !end.is_finite() || end <= start {
            return 0.0;
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

        let mut total = 0.0;
        let mut merged: Option<TimeStopInterval> = None;
        for interval in intervals {
            match merged {
                Some(mut current) if interval.start <= current.end => {
                    current.end = current.end.max(interval.end);
                    merged = Some(current);
                }
                Some(current) => {
                    total += current.end - current.start;
                    merged = Some(interval);
                }
                None => merged = Some(interval),
            }
        }
        if let Some(current) = merged {
            total += current.end - current.start;
        }
        total
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
}
