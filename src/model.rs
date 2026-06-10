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
}

#[derive(Clone, Debug)]
pub struct PacketDebug {
    pub timestamp: f64,
    pub source: String,
    pub destination: String,
    pub payload_len: usize,
    pub declared_ids: Vec<u32>,
    pub parsed_hits: usize,
    pub note: String,
    pub payload_preview: String,
}

#[derive(Clone, Debug, Default)]
pub struct CharacterStats {
    pub char_id: u32,
    pub name: String,
    pub hits: u64,
    pub damage: f64,
    pub last_hit: f64,
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
                ..Default::default()
            });
        row.name = hit.char_name.clone();
        row.hits += 1;
        row.damage += hit.damage;
        row.last_hit = hit.timestamp;
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
