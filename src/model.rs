use std::collections::{HashMap, VecDeque};

const ABYSS_RESTART_STAGE_WINDOW_SECONDS: f64 = 10.0;

use serde::{Deserialize, Serialize};

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
    pub stats: HashMap<u32, CharacterStats>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub total_damage: f64,
    pub total_damage_taken: f64,
}

impl PartyCombatState {
    pub fn push_hit(&mut self, hit: Hit) {
        let row = self
            .stats
            .entry(hit.char_id)
            .or_insert_with(|| CharacterStats {
                char_id: hit.char_id,
                name: hit.char_name.clone(),
                first_hit: hit.timestamp,
                last_hit: hit.timestamp,
                ..Default::default()
            });
        row.name = hit.char_name.clone();
        if hit.direction == "incoming" {
            row.hits_taken += 1;
            row.damage_taken += hit.damage;
            self.total_damage_taken += hit.damage;
        } else {
            self.started_at = Some(
                self.started_at
                    .map_or(hit.timestamp, |value| value.min(hit.timestamp)),
            );
            self.ended_at = Some(
                self.ended_at
                    .map_or(hit.timestamp, |value| value.max(hit.timestamp)),
            );
            self.total_damage += hit.damage;
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
        self.hits.push_back(hit);
        while self.hits.len() > 50_000 {
            self.hits.pop_front();
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
                let switched_immediately_before = self.active_half.is_some()
                    && self.last_half_switch_at.is_some_and(|switch_at| {
                        timestamp >= switch_at
                            && timestamp - switch_at <= ABYSS_RESTART_STAGE_WINDOW_SECONDS
                    })
                    && self.last_half_switch_from.is_some();
                if switched_immediately_before {
                    self.clear_restarted_floor();
                    self.pending_restart_at = None;
                    self.pending_restart_half = None;
                } else if let Some(half) = self.active_half {
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
        let row = self
            .stats
            .entry(hit.char_id)
            .or_insert_with(|| CharacterStats {
                char_id: hit.char_id,
                name: hit.char_name.clone(),
                first_hit: hit.timestamp,
                last_hit: hit.timestamp,
                ..Default::default()
            });
        row.name = hit.char_name.clone();
        if hit.direction == "incoming" {
            row.hits_taken += 1;
            row.damage_taken += hit.damage;
            self.total_damage_taken += hit.damage;
        } else {
            self.started_at = Some(
                self.started_at
                    .map_or(hit.timestamp, |v| v.min(hit.timestamp)),
            );
            self.ended_at = Some(
                self.ended_at
                    .map_or(hit.timestamp, |v| v.max(hit.timestamp)),
            );
            self.total_damage += hit.damage;
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
        self.hits.push_back(hit);
        while self.hits.len() > 50_000 {
            self.hits.pop_front();
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
        let duration = self.duration();
        if duration > 0.0 {
            self.total_damage / duration
        } else {
            0.0
        }
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
    Error(String),
    CaptureStopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(timestamp: f64, before: f64, damage: f64, max_hp: f64) -> Hit {
        Hit {
            timestamp,
            char_id: 1,
            char_name: "测试角色".to_owned(),
            char_known: true,
            damage,
            byte_offset: 0,
            bit_shift: 0,
            char_source: "test".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: before,
            target_hp_after: before - damage,
            target_max_hp: max_hp,
            target_hp_percent: (before - damage) / max_hp * 100.0,
            target_id: None,
            target_name: None,
            target_context: Vec::new(),
        }
    }

    #[test]
    fn leaves_target_empty_without_explicit_packet_identity() {
        let mut state = CombatState::default();
        state.push_hit(hit(1.0, 1_000.0, 200.0, 1_000.0));

        assert!(state.hits[0].target_id.is_none());
        assert!(state.hits[0].target_name.is_none());
    }

    #[test]
    fn calculates_character_dps_from_its_own_active_window() {
        let mut state = CombatState::default();
        state.push_hit(hit(10.0, 1_000.0, 100.0, 1_000.0));
        state.push_hit(hit(12.0, 900.0, 100.0, 1_000.0));

        let stats = &state.stats[&1];
        assert_eq!(stats.duration(), 2.0);
        assert_eq!(stats.dps(), 100.0);
    }

    #[test]
    fn tracks_incoming_damage_without_adding_it_to_output_dps() {
        let mut state = CombatState::default();
        state.push_hit(hit(1.0, 1_000.0, 100.0, 1_000.0));
        let mut incoming = hit(2.0, 500.0, 75.0, 1_000.0);
        incoming.direction = "incoming".to_owned();
        state.push_hit(incoming);

        let stats = &state.stats[&1];
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.damage, 100.0);
        assert_eq!(stats.hits_taken, 1);
        assert_eq!(stats.damage_taken, 75.0);
        assert_eq!(state.total_damage, 100.0);
        assert_eq!(state.total_damage_taken, 75.0);
    }

    #[test]
    fn routes_abyss_hits_to_independent_halves() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 1.0,
            floor: Some(12),
            half: AbyssHalf::First,
        });
        state.push_hit(hit(2.0, 1_000.0, 100.0, 1_000.0));
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 3.0,
            floor: Some(12),
            half: AbyssHalf::Second,
        });
        let mut second_hit = hit(4.0, 2_000.0, 250.0, 2_000.0);
        second_hit.char_id = 2;
        state.push_hit(second_hit);

        assert_eq!(state.abyss.floor, Some(12));
        assert_eq!(state.abyss.first_half.hits.len(), 1);
        assert_eq!(state.abyss.first_half.total_damage, 100.0);
        assert_eq!(state.abyss.second_half.hits.len(), 1);
        assert_eq!(state.abyss.second_half.total_damage, 250.0);
        assert!(state.abyss.first_half.stats.contains_key(&1));
        assert!(state.abyss.second_half.stats.contains_key(&2));
    }

    #[test]
    fn defers_abyss_restart_clear_until_the_half_is_known() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 1.0,
            floor: Some(12),
            half: AbyssHalf::First,
        });
        state.push_hit(hit(2.0, 1_000.0, 100.0, 1_000.0));
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 3.0,
            floor: Some(12),
            half: AbyssHalf::Second,
        });
        state.push_hit(hit(4.0, 1_000.0, 200.0, 1_000.0));
        state.abyss.active_half = None;

        state.apply_abyss_event(AbyssEvent::RestartDetected { timestamp: 5.0 });

        assert_eq!(state.abyss.first_half.hits.len(), 1);
        assert_eq!(state.abyss.second_half.hits.len(), 1);
        assert!(state.abyss.active_half.is_none());
        assert_eq!(state.abyss.pending_restart_at, Some(5.0));
        assert!(state.abyss.pending_restart_half.is_none());

        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 6.0,
            floor: None,
            half: AbyssHalf::Second,
        });

        assert_eq!(state.abyss.first_half.hits.len(), 1);
        assert!(state.abyss.second_half.hits.is_empty());
        assert_eq!(state.abyss.active_half, Some(AbyssHalf::Second));
        assert!(state.abyss.pending_restart_at.is_none());
    }

    #[test]
    fn clears_both_halves_when_restart_is_followed_by_an_immediate_half_switch() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 1.0,
            floor: Some(12),
            half: AbyssHalf::First,
        });
        state.push_hit(hit(2.0, 1_000.0, 100.0, 1_000.0));
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 3.0,
            floor: Some(12),
            half: AbyssHalf::Second,
        });
        state.push_hit(hit(4.0, 1_000.0, 200.0, 1_000.0));

        state.apply_abyss_event(AbyssEvent::RestartDetected { timestamp: 5.0 });
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 6.0,
            floor: None,
            half: AbyssHalf::First,
        });

        assert!(state.abyss.first_half.hits.is_empty());
        assert!(state.abyss.second_half.hits.is_empty());
        assert_eq!(state.abyss.active_half, Some(AbyssHalf::First));
        assert!(state.abyss.pending_restart_at.is_none());
        assert!(state.abyss.pending_restart_half.is_none());
    }

    #[test]
    fn clears_both_halves_when_half_switch_arrives_before_restart_marker() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 1.0,
            floor: Some(12),
            half: AbyssHalf::First,
        });
        state.push_hit(hit(2.0, 1_000.0, 100.0, 1_000.0));
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 3.0,
            floor: Some(12),
            half: AbyssHalf::Second,
        });
        state.push_hit(hit(4.0, 1_000.0, 200.0, 1_000.0));

        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 5.0,
            floor: None,
            half: AbyssHalf::First,
        });
        state.apply_abyss_event(AbyssEvent::RestartDetected { timestamp: 6.0 });

        assert!(state.abyss.first_half.hits.is_empty());
        assert!(state.abyss.second_half.hits.is_empty());
        assert_eq!(state.abyss.active_half, Some(AbyssHalf::First));
    }

    #[test]
    fn does_not_clear_the_other_half_on_a_delayed_normal_half_switch() {
        let mut state = CombatState::default();
        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 1.0,
            floor: Some(12),
            half: AbyssHalf::First,
        });
        state.push_hit(hit(2.0, 1_000.0, 100.0, 1_000.0));
        state.apply_abyss_event(AbyssEvent::RestartDetected { timestamp: 3.0 });
        state.push_hit(hit(4.0, 1_000.0, 150.0, 1_000.0));

        state.apply_abyss_event(AbyssEvent::Stage {
            timestamp: 30.0,
            floor: None,
            half: AbyssHalf::Second,
        });

        assert_eq!(state.abyss.first_half.hits.len(), 1);
        assert!(state.abyss.second_half.hits.is_empty());
        assert_eq!(state.abyss.active_half, Some(AbyssHalf::Second));
    }
}
