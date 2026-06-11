use std::collections::{HashMap, VecDeque};

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

#[derive(Default)]
pub struct CombatState {
    pub hits: VecDeque<Hit>,
    pub packets: VecDeque<PacketDebug>,
    pub stats: HashMap<u32, CharacterStats>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub total_damage: f64,
}

impl CombatState {
    pub fn push_hit(&mut self, hit: Hit) {
        self.started_at = Some(
            self.started_at
                .map_or(hit.timestamp, |v| v.min(hit.timestamp)),
        );
        self.ended_at = Some(
            self.ended_at
                .map_or(hit.timestamp, |v| v.max(hit.timestamp)),
        );
        self.total_damage += hit.damage;
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
        row.first_hit = row.first_hit.min(hit.timestamp);
        row.hits += 1;
        row.damage += hit.damage;
        row.last_hit = row.last_hit.max(hit.timestamp);
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
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Hit(Hit),
    Packet(PacketDebug),
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
}
