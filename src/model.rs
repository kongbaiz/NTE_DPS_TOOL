use std::collections::{HashMap, VecDeque};

const ABYSS_RESTART_STAGE_WINDOW_SECONDS: f64 = 10.0;

use serde::{Deserialize, Serialize};

const MAX_COMBAT_HITS: usize = 50_000;

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
        match hit.direction.as_str() {
            "incoming" => {
                summary.incoming_damage += hit.damage;
                summary.incoming_hits += 1;
            }
            "outgoing" => {
                summary.outgoing_damage += hit.damage;
                summary.outgoing_hits += 1;
            }
            _ => {
                summary.unknown_damage += hit.damage;
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

    pub fn dps(&self) -> f64 {
        self.damage / self.duration().max(1.0)
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
    if hit.direction == "incoming" {
        row.hits_taken += 1;
        row.damage_taken += hit.damage;
        *total_damage_taken += hit.damage;
        return;
    }

    *started_at = Some(started_at.map_or(hit.timestamp, |value| value.min(hit.timestamp)));
    *ended_at = Some(ended_at.map_or(hit.timestamp, |value| value.max(hit.timestamp)));
    *total_damage += hit.damage;
    if row.hits == 0 {
        row.first_hit = hit.timestamp;
        row.last_hit = hit.timestamp;
    } else {
        row.first_hit = row.first_hit.min(hit.timestamp);
        row.last_hit = row.last_hit.max(hit.timestamp);
    }
    row.hits += 1;
    row.damage += hit.damage;
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
            Self::First => "上半",
            Self::Second => "下半",
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
    },
    Success {
        timestamp: f64,
    },
    Exit {
        timestamp: f64,
    },
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
        let mut trimmed = false;
        while self.hits.len() > MAX_COMBAT_HITS {
            self.hits.pop_front();
            trimmed = true;
        }
        if trimmed {
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

    pub fn duration(&self) -> f64 {
        match (self.started_at, self.ended_at) {
            (Some(start), Some(end)) => (end - start).max(0.001),
            _ => 0.0,
        }
    }

    pub fn dps(&self) -> f64 {
        self.total_damage / self.duration().max(1.0)
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
        let mut trimmed = false;
        while self.hits.len() > MAX_COMBAT_HITS {
            self.hits.pop_front();
            trimmed = true;
        }
        if trimmed {
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

    pub fn push_packet(&mut self, packet: PacketDebug) {
        self.packets.push_back(packet);
        while self.packets.len() > 10_000 {
            self.packets.pop_front();
        }
    }

    pub fn duration(&self) -> f64 {
        match (self.started_at, self.ended_at) {
            (Some(start), Some(end)) => (end - start).max(0.001),
            _ => 0.0,
        }
    }

    pub fn dps(&self) -> f64 {
        self.total_damage / self.duration().max(1.0)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn apply_abyss_event(&mut self, event: AbyssEvent) {
        self.abyss.apply_event(event);
    }
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Hit(Hit),
    Packet(PacketDebug),
    Abyss(AbyssEvent),
    Status(String),
    Warning(String),
    Error(String),
    CaptureStopped,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    fn assert_totals_match_hits(
        hits: &VecDeque<Hit>,
        stats: &HashMap<u32, CharacterStats>,
        total_damage: f64,
        total_damage_taken: f64,
        duration: f64,
    ) {
        assert_eq!(hits.len(), MAX_COMBAT_HITS);

        let expected_damage: f64 = hits
            .iter()
            .filter(|hit| hit.direction != "incoming")
            .map(|hit| hit.damage)
            .sum();
        let expected_damage_taken: f64 = hits
            .iter()
            .filter(|hit| hit.direction == "incoming")
            .map(|hit| hit.damage)
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
                    .map(|hit| hit.damage)
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
                    .map(|hit| hit.damage)
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
    fn abyss_half_labels_are_utf8_chinese() {
        assert_eq!(AbyssHalf::First.label(), "上半");
        assert_eq!(AbyssHalf::Second.label(), "下半");
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
            state.duration(),
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
            state.duration(),
        );
        assert!(!state.stats.contains_key(&99));
    }
}
