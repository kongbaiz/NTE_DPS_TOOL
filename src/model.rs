use std::collections::{HashMap, HashSet, VecDeque};

const ABYSS_RESTART_STAGE_WINDOW_SECONDS: f64 = 10.0;
const TARGET_NAME_BACKFILL_WINDOW_SECONDS: f64 = 30.0;

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
pub struct HitTargetUpdate {
    pub hit_uid: String,
    pub timestamp: f64,
    pub char_id: u32,
    pub damage: f64,
    pub byte_offset: usize,
    pub bit_shift: u8,
    pub target_id: Option<String>,
    pub target_name: Option<String>,
    pub target_context: Vec<String>,
    pub target_score: i32,
    pub target_confidence: String,
    pub old_target_id: Option<String>,
    pub update_reason: Option<String>,
    pub update_strength: Option<String>,
    pub target_generation: Option<String>,
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

    fn apply_target_update(&mut self, update: &HitTargetUpdate) -> bool {
        if apply_target_update_to_hits(&mut self.hits, update) {
            self.hits_generation = self.hits_generation.wrapping_add(1);
            true
        } else {
            false
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

    pub fn apply_target_update(&mut self, update: &HitTargetUpdate) -> bool {
        let mut changed = self.first_half.apply_target_update(update);
        changed |= self.second_half.apply_target_update(update);
        changed
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

    pub fn apply_target_update(&mut self, update: HitTargetUpdate) {
        let mut changed = self.abyss.apply_target_update(&update);
        changed |= apply_target_update_to_hits(&mut self.hits, &update);
        if changed {
            self.hits_generation = self.hits_generation.wrapping_add(1);
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
    HitTargetUpdate(HitTargetUpdate),
    Packet(PacketDebug),
    Abyss(AbyssEvent),
    Status(String),
    Warning(String),
    Error(String),
    CaptureStopped,
}

fn hit_matches_update(hit: &Hit, update: &HitTargetUpdate) -> bool {
    let computed_uid = stable_hit_uid(hit);
    if !update.hit_uid.is_empty() {
        return update.hit_uid == computed_uid;
    }
    hit.timestamp.to_bits() == update.timestamp.to_bits()
        && hit.char_id == update.char_id
        && hit.damage.to_bits() == update.damage.to_bits()
        && hit.byte_offset == update.byte_offset
        && hit.bit_shift == update.bit_shift
}

pub fn stable_hit_uid(hit: &Hit) -> String {
    format!(
        "{:016x}:{:08x}:{:016x}:{}:{}:{:016x}:{:016x}",
        hit.timestamp.to_bits(),
        hit.char_id,
        hit.damage.to_bits(),
        hit.byte_offset,
        hit.bit_shift,
        hit.target_hp_before.to_bits(),
        hit.target_hp_after.to_bits()
    )
}

struct TargetBackfillRule {
    timestamp: f64,
    target_id: Option<String>,
    target_name: String,
    target_path: Option<String>,
    alias_keys: HashSet<String>,
}

impl TargetBackfillRule {
    fn from_update(update: &HitTargetUpdate) -> Option<Self> {
        if has_unreliable_target_name_source(&update.target_context)
            || !has_runtime_alias_target_source(&update.target_context)
        {
            return None;
        }
        let target_name = update.target_name.as_ref()?.trim();
        if target_name.is_empty() {
            return None;
        }
        Some(Self {
            timestamp: update.timestamp,
            target_id: update.target_id.clone(),
            target_name: target_name.to_owned(),
            target_path: target_context_value(&update.target_context, "target_path")
                .map(str::to_owned),
            alias_keys: target_alias_lookup_keys(update.target_id.as_ref(), &update.target_context),
        })
    }

    fn matches_alias(&self, hit: &Hit) -> bool {
        if self.alias_keys.is_empty() {
            return false;
        }
        if let Some(rule_path) = self.target_path.as_deref()
            && let Some(hit_path) = target_context_value(&hit.target_context, "target_path")
            && !hit_path.eq_ignore_ascii_case(rule_path)
        {
            return false;
        }
        target_alias_lookup_keys(hit.target_id.as_ref(), &hit.target_context)
            .iter()
            .any(|key| self.alias_keys.contains(key))
    }
}

fn apply_target_update_to_hits(hits: &mut VecDeque<Hit>, update: &HitTargetUpdate) -> bool {
    let _ = update.target_score;
    let _ = update.target_confidence.as_str();
    let _ = update.old_target_id.as_deref();
    let _ = update.update_reason.as_deref();
    let _ = update.update_strength.as_deref();
    let _ = update.target_generation.as_deref();
    let mut changed = false;
    for hit in hits
        .iter_mut()
        .filter(|hit| hit_matches_update(hit, update))
    {
        changed |= apply_exact_target_update(hit, update);
    }

    let Some(rule) = TargetBackfillRule::from_update(update) else {
        return changed;
    };
    let allow_alias_backfill = !has_conflicting_named_target(hits, &rule);
    for hit in hits.iter_mut() {
        if hit.direction == "incoming"
            || hit.target_name.is_some()
            || !can_backfill_target_name(hit)
        {
            continue;
        }
        if allow_alias_backfill && rule.matches_alias(hit) {
            changed |= apply_target_backfill_rule(hit, &rule);
        }
    }
    changed
}

fn apply_exact_target_update(hit: &mut Hit, update: &HitTargetUpdate) -> bool {
    if has_recent_death_suppressed_target(&hit.target_context) && update.target_name.is_some() {
        return false;
    }
    if let (Some(old_name), Some(new_name)) =
        (hit.target_name.as_deref(), update.target_name.as_deref())
        && old_name != new_name
    {
        return push_unique_context(
            &mut hit.target_context,
            "target_conflict=locked_name_mismatch".to_owned(),
        );
    }
    if hit.target_id.is_some()
        && update.target_id.is_some()
        && hit.target_id != update.target_id
        && !update_has_authoritative_lifecycle_reset(update)
    {
        return push_unique_context(
            &mut hit.target_context,
            "target_conflict=locked_path_mismatch".to_owned(),
        );
    }
    let changed = hit.target_id != update.target_id
        || hit.target_name != update.target_name
        || hit.target_context != update.target_context;
    if changed {
        hit.target_id = update.target_id.clone();
        hit.target_name = update.target_name.clone();
        hit.target_context = update.target_context.clone();
    }
    changed
}

fn update_has_authoritative_lifecycle_reset(update: &HitTargetUpdate) -> bool {
    update
        .target_context
        .iter()
        .any(|entry| entry.starts_with("target_lifecycle_reset="))
}

fn apply_target_backfill_rule(hit: &mut Hit, rule: &TargetBackfillRule) -> bool {
    let mut changed = false;
    if hit.target_id.is_none() && rule.target_id.is_some() {
        hit.target_id.clone_from(&rule.target_id);
        changed = true;
    }
    if hit.target_name.as_deref() != Some(rule.target_name.as_str()) {
        hit.target_name = Some(rule.target_name.clone());
        changed = true;
    }
    if let Some(target_path) = &rule.target_path {
        changed |= replace_or_push_context(&mut hit.target_context, "target_path", target_path);
    }
    changed |= replace_or_push_context(&mut hit.target_context, "target_name", &rule.target_name);
    changed |= replace_or_push_context(
        &mut hit.target_context,
        "target_name_resolution",
        "state_backfill",
    );
    changed
}

fn can_backfill_target_name(hit: &Hit) -> bool {
    hit.target_hp_after > 1.0 && !has_unreliable_target_name_source(&hit.target_context)
}

fn has_recent_death_suppressed_target(context: &[String]) -> bool {
    context
        .iter()
        .any(|entry| entry == "reason=recent_death_suppressed_stale_target")
}

fn has_unreliable_target_name_source(context: &[String]) -> bool {
    context.iter().any(|entry| {
        matches!(
            entry.as_str(),
            "reason=recent_death_suppressed_stale_target"
                | "reason=hp_handle_path_without_direct_hp_suppressed"
                | "reason=path_only_target_name_suppressed"
                | "reason=net_identity_path_anchor_unconfirmed"
                | "reason=runtime_hp_timeline"
                | "reason=runtime_unique_active_named_instance"
        ) || entry == "target_name_resolution=handle_alias_applied"
            || entry == "target_name_resolution=state_backfill"
    })
}

fn has_runtime_alias_target_source(context: &[String]) -> bool {
    context
        .iter()
        .any(|entry| entry.starts_with("reason=runtime_alias:"))
}

fn has_conflicting_named_target(hits: &VecDeque<Hit>, rule: &TargetBackfillRule) -> bool {
    hits.iter()
        .filter(|hit| hit.direction != "incoming")
        .filter(|hit| (hit.timestamp - rule.timestamp).abs() <= TARGET_NAME_BACKFILL_WINDOW_SECONDS)
        .filter_map(|hit| hit.target_name.as_deref())
        .any(|name| name != rule.target_name)
}

fn target_context_value<'a>(context: &'a [String], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    context
        .iter()
        .find_map(|value| value.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "None")
}

fn target_context_values<'a>(context: &'a [String], key: &str) -> impl Iterator<Item = &'a str> {
    let prefix = format!("{key}=");
    context
        .iter()
        .filter_map(move |value| value.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "None")
}

fn replace_or_push_context(context: &mut Vec<String>, key: &str, value: &str) -> bool {
    let prefix = format!("{key}=");
    let next = format!("{key}={value}");
    let mut changed = false;
    context.retain(|entry| {
        let keep = !entry.starts_with(&prefix);
        changed |= !keep;
        keep
    });
    if !context.iter().any(|entry| entry == &next) {
        context.push(next);
        changed = true;
    }
    changed
}

fn push_unique_context(context: &mut Vec<String>, value: String) -> bool {
    if context.iter().any(|entry| entry == &value) {
        return false;
    }
    context.push(value);
    true
}

fn target_alias_lookup_keys(target_id: Option<&String>, context: &[String]) -> HashSet<String> {
    let mut keys = HashSet::new();
    if let Some(target_id) = target_id {
        extend_equivalent_target_alias_keys(&mut keys, target_id);
    }
    for value in target_context_values(context, "target_handle_candidate") {
        extend_equivalent_target_alias_keys(&mut keys, value);
    }
    for value in target_context_values(context, "boss_hp_guid") {
        extend_equivalent_target_alias_keys(&mut keys, &format!("boss_hp_guid:{value}"));
    }
    for value in target_context_values(context, "current_hp_token") {
        extend_equivalent_target_alias_keys(&mut keys, &format!("current_hp_token:{value}"));
    }
    for key in [
        "actor_channel",
        "iris_ref32",
        "netguid32",
        "netguid_packed",
        "sdk_net_target",
    ] {
        for value in target_context_values(context, key) {
            extend_equivalent_target_alias_keys(&mut keys, &format!("{key}:{value}"));
        }
    }
    keys
}

fn extend_equivalent_target_alias_keys(keys: &mut HashSet<String>, key: &str) {
    for key in equivalent_target_alias_keys(key) {
        keys.insert(key);
    }
}

fn equivalent_target_alias_keys(key: &str) -> Vec<String> {
    let key = normalize_target_alias_key(key);
    let mut keys = vec![key.clone()];
    if let Some(value) = key.strip_prefix("AttributeGuid:") {
        keys.push(format!("boss_hp_guid:{value}"));
    } else if let Some(value) = key.strip_prefix("boss_hp_guid:") {
        keys.push(format!("AttributeGuid:{value}"));
    } else if let Some(value) = key.strip_prefix("NetRefHandleCandidate:currenthp:") {
        keys.push(format!("current_hp_token:{value}"));
    } else if let Some(value) = key.strip_prefix("current_hp_token:") {
        keys.push(format!("NetRefHandleCandidate:currenthp:{value}"));
    } else if let Some(value) = key.strip_prefix("NetRefHandleCandidate:sdk_target:") {
        keys.push(format!("sdk_net_target:{value}"));
    } else if let Some(value) = key.strip_prefix("sdk_net_target:") {
        keys.push(format!("NetRefHandleCandidate:sdk_target:{value}"));
    } else if let Some(value) = key.strip_prefix("NetRefHandleCandidate:") {
        keys.push(format!("iris_ref32:{value}"));
    } else if let Some(value) = key.strip_prefix("iris_ref32:") {
        keys.push(format!("NetRefHandleCandidate:{value}"));
    } else if let Some(value) = key.strip_prefix("NetGuidCandidate:") {
        keys.push(format!("netguid32:{value}"));
        keys.push(format!("netguid_packed:{value}"));
    } else if let Some(value) = key.strip_prefix("netguid32:") {
        keys.push(format!("NetGuidCandidate:{value}"));
        keys.push(format!("netguid_packed:{value}"));
    } else if let Some(value) = key.strip_prefix("netguid_packed:") {
        keys.push(format!("NetGuidCandidate:{value}"));
        keys.push(format!("netguid32:{value}"));
    }
    keys
}

fn normalize_target_alias_key(key: &str) -> String {
    let key = key.trim().split('|').next().unwrap_or(key.trim());
    let Some((kind, value)) = key.split_once(':') else {
        return key.to_owned();
    };
    format!("{kind}:{}", value.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit_with_target(name: Option<&str>, id: Option<&str>) -> Hit {
        Hit {
            timestamp: 1.0,
            char_id: 7,
            char_name: "tester".to_owned(),
            char_known: true,
            damage: 100.0,
            byte_offset: 22,
            bit_shift: 1,
            char_source: "test".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: 1000.0,
            target_hp_after: 900.0,
            target_max_hp: 1000.0,
            target_hp_percent: 90.0,
            target_id: id.map(str::to_owned),
            target_name: name.map(str::to_owned),
            target_context: Vec::new(),
            gameplay_effect_index: None,
            gameplay_effect_name: None,
            ability_name: None,
            damage_name: None,
            attack_type: None,
        }
    }

    #[test]
    fn exact_update_cannot_rename_locked_hit() {
        let mut hits = VecDeque::from([hit_with_target(Some("Target A"), Some("a#1"))]);
        let original_uid = stable_hit_uid(hits.front().expect("hit"));
        let update = HitTargetUpdate {
            hit_uid: original_uid,
            timestamp: 1.0,
            char_id: 7,
            damage: 100.0,
            byte_offset: 22,
            bit_shift: 1,
            target_id: Some("b#1".to_owned()),
            target_name: Some("Target B".to_owned()),
            target_context: vec!["target_path=/Game/Monster/B".to_owned()],
            target_score: 120,
            target_confidence: "confirmed".to_owned(),
            old_target_id: Some("a#1".to_owned()),
            update_reason: Some("test".to_owned()),
            update_strength: Some("confirmed".to_owned()),
            target_generation: Some("1".to_owned()),
        };

        assert!(apply_target_update_to_hits(&mut hits, &update));
        let hit = hits.front().expect("hit after update");
        assert_eq!(hit.target_name.as_deref(), Some("Target A"));
        assert_eq!(hit.target_id.as_deref(), Some("a#1"));
        assert!(
            hit.target_context
                .iter()
                .any(|entry| { entry == "target_conflict=locked_name_mismatch" })
        );
    }
}
