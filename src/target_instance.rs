use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::model::Hit;
use crate::net_identity::{NetIdentityCandidate, NetIdentityCandidateKind};
use crate::object_state::is_ignored_non_target_path;
use crate::resource_index::ResourceIndex;
use crate::target_resolver::TargetConfidence;
use crate::ue_bitstream::PathCandidate;

const MAX_HP_HISTORY_PER_INSTANCE: usize = 32;
const INSTANCE_PENDING_WINDOW_SECONDS: f64 = 60.0;
const INSTANCE_ACTIVE_TTL_SECONDS: f64 = 90.0;
const HP_MATCH_TOLERANCE_ABSOLUTE: f64 = 2.0;
const HP_MATCH_TOLERANCE_RATIO: f64 = 0.002;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TargetAliasKind {
    ActorChannel,
    IrisRef32,
    NetGuid32,
    NetGuidPacked,
    SdkNetTarget,
    BossHpGuid,
    CurrentHpToken,
    HitTargetToken,
    HitVectorToken,
}

impl TargetAliasKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ActorChannel => "actor_channel",
            Self::IrisRef32 => "iris_ref32",
            Self::NetGuid32 => "netguid32",
            Self::NetGuidPacked => "netguid_packed",
            Self::SdkNetTarget => "sdk_net_target",
            Self::BossHpGuid => "boss_hp_guid",
            Self::CurrentHpToken => "current_hp_token",
            Self::HitTargetToken => "hit_target_token",
            Self::HitVectorToken => "hit_target_vector_token",
        }
    }

    fn instance_id_priority(self) -> u8 {
        match self {
            Self::IrisRef32 => 8,
            Self::NetGuid32 => 7,
            Self::NetGuidPacked => 6,
            Self::ActorChannel => 5,
            Self::SdkNetTarget => 4,
            Self::BossHpGuid => 4,
            Self::CurrentHpToken => 3,
            Self::HitTargetToken => 2,
            Self::HitVectorToken => 1,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TargetAlias {
    pub kind: TargetAliasKind,
    pub value: String,
}

impl TargetAlias {
    pub fn new(kind: TargetAliasKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: normalize_alias_value(value.into()),
        }
    }

    pub fn key(&self) -> String {
        alias_key(self.kind, &self.value)
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct RuntimeTargetHpObservation {
    pub timestamp: f64,
    pub current: f64,
    pub evidence: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeTargetState {
    Active,
    Dead,
    Expired,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct RuntimeTargetInstance {
    pub instance_id: String,
    pub canonical_path: String,
    pub target_name: String,
    pub spawn_seq: u32,
    pub first_seen_at: f64,
    pub last_seen_at: f64,
    pub aliases: BTreeSet<TargetAlias>,
    pub hp_current: Option<f64>,
    pub hp_history: VecDeque<RuntimeTargetHpObservation>,
    pub state: RuntimeTargetState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetInstanceResolution {
    pub instance_id: String,
    pub target_name: String,
    pub canonical_path: String,
    pub confidence: TargetConfidence,
    pub score: i32,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct TargetInstanceStore {
    instances: HashMap<String, RuntimeTargetInstance>,
    alias_index: HashMap<String, String>,
    spawn_seq_by_path: HashMap<String, u32>,
}

impl TargetInstanceStore {
    pub fn observe_paths(
        &mut self,
        timestamp: f64,
        paths: &[PathCandidate],
        identities: &[NetIdentityCandidate],
        resources: &ResourceIndex,
    ) -> Vec<String> {
        let mut notes = Vec::new();
        for path in paths {
            if is_ignored_non_target_path(&path.value) {
                notes.push(format!("ignored_non_target_path={}", path.value));
                continue;
            }
            let Some((canonical_path, target_name)) =
                resolved_target_path_name(resources, &path.value)
            else {
                continue;
            };
            let aliases = identities
                .iter()
                .filter(|identity| identity.path == path.value)
                .filter_map(alias_from_net_identity)
                .collect::<Vec<_>>();
            let instance_id =
                self.observe_monster_actor(timestamp, canonical_path, target_name, aliases);
            notes.push(format!("runtime_target_instance={instance_id}"));
        }
        self.expire_old(timestamp);
        notes
    }

    pub fn observe_runtime_mapping(
        &mut self,
        timestamp: f64,
        canonical_path: String,
        target_name: String,
        aliases: Vec<TargetAlias>,
    ) -> String {
        let instance_id =
            self.observe_monster_actor(timestamp, canonical_path, target_name, aliases);
        self.expire_old(timestamp);
        instance_id
    }

    pub fn close_alias(
        &mut self,
        timestamp: f64,
        alias: &TargetAlias,
        expire_instance: bool,
    ) -> Option<String> {
        let instance_id = self.alias_index.remove(&alias.key())?;
        let instance = self.instances.get_mut(&instance_id)?;
        instance.aliases.remove(alias);
        instance.last_seen_at = timestamp;
        if expire_instance {
            instance.state = RuntimeTargetState::Expired;
        }
        Some(instance.instance_id.clone())
    }

    pub fn expire_path(&mut self, timestamp: f64, canonical_path: &str) -> Vec<String> {
        let instance_ids = self
            .instances
            .values()
            .filter(|instance| instance.canonical_path == canonical_path)
            .filter(|instance| instance.state == RuntimeTargetState::Active)
            .map(|instance| instance.instance_id.clone())
            .collect::<Vec<_>>();
        let mut removed_alias_keys = Vec::new();
        for instance_id in instance_ids {
            let Some(instance) = self.instances.get_mut(&instance_id) else {
                continue;
            };
            instance.last_seen_at = timestamp;
            instance.state = RuntimeTargetState::Expired;
            for alias in &instance.aliases {
                let key = alias.key();
                self.alias_index.remove(&key);
                removed_alias_keys.push(key);
            }
        }
        removed_alias_keys
    }

    pub fn observe_boss_hp_guid(
        &mut self,
        timestamp: f64,
        handle: [u8; 16],
        current_hp: f64,
        evidence: String,
    ) -> Option<String> {
        let alias = TargetAlias::new(TargetAliasKind::BossHpGuid, hex::encode(handle));
        self.observe_hp_alias(timestamp, alias, current_hp, evidence)
    }

    pub fn observe_current_hp_token(
        &mut self,
        timestamp: f64,
        token: &[u8],
        current_hp: f64,
        evidence: String,
    ) -> Option<String> {
        let alias = TargetAlias::new(TargetAliasKind::CurrentHpToken, hex::encode(token));
        self.observe_hp_alias(timestamp, alias, current_hp, evidence)
    }

    pub fn observe_sdk_net_target(
        &mut self,
        timestamp: f64,
        token: &[u8],
        current_hp: f64,
        evidence: String,
    ) -> Option<String> {
        let alias = TargetAlias::new(TargetAliasKind::SdkNetTarget, hex::encode(token));
        self.observe_hp_alias(timestamp, alias, current_hp, evidence)
    }

    pub fn resolve_hit(&self, hit: &Hit) -> Option<TargetInstanceResolution> {
        for alias in aliases_from_hit_context(hit) {
            if let Some(instance) = self.instance_for_alias(&alias) {
                return Some(instance_resolution(
                    instance,
                    TargetConfidence::Confirmed,
                    120,
                    format!("runtime_alias:{}", alias.key()),
                ));
            }
        }
        if let Some(instance) = self.resolve_by_hp_timeline(hit) {
            return Some(instance_resolution(
                instance,
                TargetConfidence::Probable,
                90,
                "runtime_hp_timeline".to_owned(),
            ));
        }
        let active = self
            .active_named_instances(hit.timestamp)
            .collect::<Vec<_>>();
        if active.len() == 1 {
            return Some(instance_resolution(
                active[0],
                TargetConfidence::Possible,
                45,
                "runtime_unique_active_named_instance".to_owned(),
            ));
        }
        None
    }

    pub fn instance_for_alias(&self, alias: &TargetAlias) -> Option<&RuntimeTargetInstance> {
        self.alias_index
            .get(&alias.key())
            .and_then(|instance_id| self.instances.get(instance_id))
    }

    #[allow(dead_code)]
    pub fn instance(&self, instance_id: &str) -> Option<&RuntimeTargetInstance> {
        self.instances.get(instance_id)
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    fn observe_monster_actor(
        &mut self,
        timestamp: f64,
        canonical_path: String,
        target_name: String,
        aliases: Vec<TargetAlias>,
    ) -> String {
        let existing_id = aliases
            .iter()
            .find_map(|alias| self.alias_index.get(&alias.key()).cloned())
            .or_else(|| self.recent_unaliased_instance_id(&canonical_path, timestamp));
        let instance_id = if let Some(instance_id) = existing_id {
            instance_id
        } else {
            self.create_instance(
                timestamp,
                canonical_path.clone(),
                target_name.clone(),
                &aliases,
            )
        };
        let mut current_id = instance_id.clone();
        for alias in aliases {
            current_id = self.add_alias_and_maybe_rename(&current_id, alias);
        }
        if let Some(instance) = self.instances.get_mut(&current_id) {
            instance.last_seen_at = timestamp;
            instance.target_name = target_name;
            instance.canonical_path = canonical_path;
            if instance.state == RuntimeTargetState::Expired {
                instance.state = RuntimeTargetState::Active;
            }
            return instance.instance_id.clone();
        }
        instance_id
    }

    fn create_instance(
        &mut self,
        timestamp: f64,
        canonical_path: String,
        target_name: String,
        aliases: &[TargetAlias],
    ) -> String {
        let spawn_seq = self
            .spawn_seq_by_path
            .entry(canonical_path.clone())
            .and_modify(|value| *value += 1)
            .or_insert(1);
        let instance_id = preferred_instance_id(&canonical_path, *spawn_seq, aliases);
        self.instances.insert(
            instance_id.clone(),
            RuntimeTargetInstance {
                instance_id: instance_id.clone(),
                canonical_path,
                target_name,
                spawn_seq: *spawn_seq,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                aliases: BTreeSet::new(),
                hp_current: None,
                hp_history: VecDeque::new(),
                state: RuntimeTargetState::Active,
            },
        );
        instance_id
    }

    fn observe_hp_alias(
        &mut self,
        timestamp: f64,
        alias: TargetAlias,
        current_hp: f64,
        evidence: String,
    ) -> Option<String> {
        let instance_id = self
            .alias_index
            .get(&alias.key())
            .cloned()
            .or_else(|| self.best_pending_instance_id(timestamp, &alias))?;
        let current_id = self.add_alias_and_maybe_rename(&instance_id, alias);
        let instance = self.instances.get_mut(&current_id)?;
        instance.last_seen_at = timestamp;
        instance.hp_current = Some(current_hp);
        if current_hp <= 0.0 {
            instance.state = RuntimeTargetState::Dead;
        }
        instance.hp_history.push_back(RuntimeTargetHpObservation {
            timestamp,
            current: current_hp,
            evidence,
        });
        while instance.hp_history.len() > MAX_HP_HISTORY_PER_INSTANCE {
            instance.hp_history.pop_front();
        }
        Some(instance.instance_id.clone())
    }

    fn add_alias_and_maybe_rename(&mut self, instance_id: &str, alias: TargetAlias) -> String {
        let current_id = self.current_id_for(instance_id);
        let Some(instance) = self.instances.get_mut(&current_id) else {
            return current_id;
        };
        instance.aliases.insert(alias.clone());
        self.alias_index.insert(alias.key(), current_id.clone());
        let best_id = preferred_instance_id(
            &instance.canonical_path,
            instance.spawn_seq,
            &instance.aliases.iter().cloned().collect::<Vec<_>>(),
        );
        if best_id == current_id || self.instances.contains_key(&best_id) {
            return current_id;
        }
        let mut instance = self
            .instances
            .remove(&current_id)
            .expect("instance existed before rename");
        instance.instance_id = best_id.clone();
        for alias in &instance.aliases {
            self.alias_index.insert(alias.key(), best_id.clone());
        }
        self.instances.insert(best_id.clone(), instance);
        best_id
    }

    fn current_id_for(&self, instance_id: &str) -> String {
        if self.instances.contains_key(instance_id) {
            return instance_id.to_owned();
        }
        self.instances
            .values()
            .find(|instance| {
                instance.aliases.iter().any(|alias| {
                    self.alias_index
                        .get(&alias.key())
                        .is_some_and(|current| current == instance.instance_id.as_str())
                })
            })
            .map(|instance| instance.instance_id.clone())
            .unwrap_or_else(|| instance_id.to_owned())
    }

    fn recent_unaliased_instance_id(&self, canonical_path: &str, timestamp: f64) -> Option<String> {
        self.instances
            .values()
            .filter(|instance| instance.canonical_path == canonical_path)
            .filter(|instance| instance.aliases.is_empty())
            .filter(|instance| timestamp - instance.last_seen_at <= 1.0)
            .max_by(|left, right| left.last_seen_at.total_cmp(&right.last_seen_at))
            .map(|instance| instance.instance_id.clone())
    }

    fn best_pending_instance_id(&self, timestamp: f64, alias: &TargetAlias) -> Option<String> {
        let mut candidates = self
            .instances
            .values()
            .filter_map(|instance| {
                let score = pending_instance_score(instance, timestamp, alias)?;
                Some((score, instance))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.last_seen_at.total_cmp(&left.1.last_seen_at))
        });
        let (best_score, best) = candidates.first()?;
        let Some((second_score, _)) = candidates.get(1) else {
            return Some(best.instance_id.clone());
        };
        (best_score - second_score >= 25).then(|| best.instance_id.clone())
    }

    fn active_named_instances(
        &self,
        timestamp: f64,
    ) -> impl Iterator<Item = &RuntimeTargetInstance> {
        self.instances.values().filter(move |instance| {
            instance.state == RuntimeTargetState::Active
                && timestamp - instance.last_seen_at <= INSTANCE_ACTIVE_TTL_SECONDS
                && !instance.target_name.is_empty()
        })
    }

    fn resolve_by_hp_timeline(&self, hit: &Hit) -> Option<&RuntimeTargetInstance> {
        let mut matched = self
            .instances
            .values()
            .filter(|instance| {
                instance
                    .hp_history
                    .as_slices()
                    .0
                    .windows(2)
                    .any(|pair| hp_pair_matches_hit(&pair[0], &pair[1], hit))
                    || instance
                        .hp_history
                        .as_slices()
                        .1
                        .windows(2)
                        .any(|pair| hp_pair_matches_hit(&pair[0], &pair[1], hit))
            })
            .collect::<Vec<_>>();
        matched.sort_by(|left, right| right.last_seen_at.total_cmp(&left.last_seen_at));
        if matched.len() == 1 {
            Some(matched[0])
        } else {
            None
        }
    }

    fn expire_old(&mut self, timestamp: f64) {
        for instance in self.instances.values_mut() {
            if instance.state == RuntimeTargetState::Active
                && timestamp - instance.last_seen_at > INSTANCE_ACTIVE_TTL_SECONDS
            {
                instance.state = RuntimeTargetState::Expired;
            }
        }
    }
}

fn resolved_target_path_name(resources: &ResourceIndex, path: &str) -> Option<(String, String)> {
    let canonical_path = resources
        .canonical_target_path_for_path(path)
        .unwrap_or_else(|| path.to_owned());
    let target_name = resources
        .resolved_name_for_path(path)
        .or_else(|| resources.resolved_name_for_path(&canonical_path))?;
    Some((canonical_path, target_name))
}

fn alias_from_net_identity(candidate: &NetIdentityCandidate) -> Option<TargetAlias> {
    let kind = match candidate.kind {
        NetIdentityCandidateKind::NetGuidPacked => TargetAliasKind::NetGuidPacked,
        NetIdentityCandidateKind::NetGuid32 => TargetAliasKind::NetGuid32,
        NetIdentityCandidateKind::IrisNetRefHandle32 => TargetAliasKind::IrisRef32,
    };
    Some(TargetAlias::new(kind, candidate.handle.clone()))
}

fn preferred_instance_id(canonical_path: &str, spawn_seq: u32, aliases: &[TargetAlias]) -> String {
    aliases
        .iter()
        .max_by_key(|alias| alias.kind.instance_id_priority())
        .map(|alias| alias.key())
        .unwrap_or_else(|| format!("{canonical_path}#{spawn_seq}"))
}

fn alias_key(kind: TargetAliasKind, value: &str) -> String {
    format!(
        "{}:{}",
        kind.label(),
        normalize_alias_value(value.to_owned())
    )
}

fn pending_instance_score(
    instance: &RuntimeTargetInstance,
    timestamp: f64,
    incoming_alias: &TargetAlias,
) -> Option<i32> {
    if is_ignored_non_target_path(&instance.canonical_path) || instance.target_name.is_empty() {
        return None;
    }
    if instance
        .aliases
        .iter()
        .any(|alias| alias.kind == incoming_alias.kind && alias.value != incoming_alias.value)
    {
        return None;
    }
    let age = timestamp - instance.last_seen_at;
    if !(0.0..=INSTANCE_PENDING_WINDOW_SECONDS).contains(&age) {
        return None;
    }
    let mut score = 100;
    score += match instance.state {
        RuntimeTargetState::Active => 80,
        RuntimeTargetState::Dead => 20,
        RuntimeTargetState::Expired => 0,
    };
    score += ((INSTANCE_PENDING_WINDOW_SECONDS - age).max(0.0) * 2.0).round() as i32;
    if instance.canonical_path.contains("Boss") || instance.canonical_path.contains("boss") {
        score += 20;
    }
    if !instance.aliases.is_empty() {
        score += 10;
    }
    Some(score)
}

fn normalize_alias_value(value: String) -> String {
    value.trim().to_ascii_lowercase()
}

fn aliases_from_hit_context(hit: &Hit) -> Vec<TargetAlias> {
    hit.target_context
        .iter()
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            let kind = match key {
                "iris_ref32" => TargetAliasKind::IrisRef32,
                "actor_channel" => TargetAliasKind::ActorChannel,
                "netguid32" => TargetAliasKind::NetGuid32,
                "netguid_packed" => TargetAliasKind::NetGuidPacked,
                "sdk_net_target" => TargetAliasKind::SdkNetTarget,
                "boss_hp_guid" => TargetAliasKind::BossHpGuid,
                "current_hp_token" => TargetAliasKind::CurrentHpToken,
                "hit_target_token" => TargetAliasKind::HitTargetToken,
                "hit_target_vector_token" => TargetAliasKind::HitVectorToken,
                _ => return None,
            };
            Some(TargetAlias::new(kind, value))
        })
        .collect()
}

fn instance_resolution(
    instance: &RuntimeTargetInstance,
    confidence: TargetConfidence,
    score: i32,
    reason: String,
) -> TargetInstanceResolution {
    TargetInstanceResolution {
        instance_id: instance.instance_id.clone(),
        target_name: instance.target_name.clone(),
        canonical_path: instance.canonical_path.clone(),
        confidence,
        score,
        reason,
    }
}

fn hp_pair_matches_hit(
    previous: &RuntimeTargetHpObservation,
    current: &RuntimeTargetHpObservation,
    hit: &Hit,
) -> bool {
    let delta = previous.current - current.current;
    let time_delta = (current.timestamp - hit.timestamp).abs();
    time_delta <= 1.0
        && nearly_equal(delta, hit.damage, hit.damage)
        && nearly_equal(previous.current, hit.target_hp_before, hit.target_hp_before)
        && nearly_equal(current.current, hit.target_hp_after, hit.target_hp_after)
}

fn nearly_equal(left: f64, right: f64, scale: f64) -> bool {
    (left - right).abs() <= HP_MATCH_TOLERANCE_ABSOLUTE.max(scale.abs() * HP_MATCH_TOLERANCE_RATIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> PathCandidate {
        PathCandidate {
            value: value.to_owned(),
            byte_offset: 0,
            bit_shift: 0,
            score: 240,
        }
    }

    fn identity(path: &str, kind: NetIdentityCandidateKind, handle: &str) -> NetIdentityCandidate {
        NetIdentityCandidate {
            kind,
            handle: handle.to_owned(),
            path: path.to_owned(),
            byte_offset: 0,
            bit_shift: 0,
            relative_offset: -4,
            raw_hex: "01020304".to_owned(),
            score: 90,
        }
    }

    fn hit() -> Hit {
        Hit {
            timestamp: 10.0,
            char_id: 1,
            char_name: "test".to_owned(),
            char_known: true,
            damage: 100.0,
            byte_offset: 0,
            bit_shift: 0,
            char_source: "test".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: 1000.0,
            target_hp_after: 900.0,
            target_max_hp: 1000.0,
            target_hp_percent: 90.0,
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

    #[test]
    fn resolved_path_creates_instance_with_preferred_iris_id() {
        let resources = ResourceIndex::load_default();
        let mut store = TargetInstanceStore::default();
        store.observe_paths(
            1.0,
            &[path("mon_01_BP")],
            &[identity(
                "mon_01_BP",
                NetIdentityCandidateKind::IrisNetRefHandle32,
                "0x81a599e2",
            )],
            &resources,
        );

        let instance = store.instance("iris_ref32:0x81a599e2").unwrap();
        assert_eq!(instance.canonical_path, "mon_01_BP");
        assert_eq!(instance.target_name, "低语种");
    }

    #[test]
    fn ignored_non_target_path_does_not_create_instance() {
        let resources = ResourceIndex::load_default();
        let mut store = TargetInstanceStore::default();
        store.observe_paths(
            1.0,
            &[path("Default__Buff_Boss07_Night_Weaktime_C")],
            &[],
            &resources,
        );

        assert_eq!(store.len(), 0);
    }

    #[test]
    fn same_path_different_iris_refs_create_separate_waves() {
        let resources = ResourceIndex::load_default();
        let mut store = TargetInstanceStore::default();
        store.observe_paths(
            1.0,
            &[path("mon_01_BP")],
            &[identity(
                "mon_01_BP",
                NetIdentityCandidateKind::IrisNetRefHandle32,
                "0x81a599e2",
            )],
            &resources,
        );
        store.observe_paths(
            30.0,
            &[path("mon_01_BP")],
            &[identity(
                "mon_01_BP",
                NetIdentityCandidateKind::IrisNetRefHandle32,
                "0x81a599f9",
            )],
            &resources,
        );

        assert!(store.instance("iris_ref32:0x81a599e2").is_some());
        assert!(store.instance("iris_ref32:0x81a599f9").is_some());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn late_boss_hp_guid_binds_unique_pending_instance() {
        let resources = ResourceIndex::load_default();
        let mut store = TargetInstanceStore::default();
        store.observe_paths(1.0, &[path("Boss_07_BP_WorldBoss")], &[], &resources);
        let handle = [
            0xc5, 0x5c, 0x88, 0x59, 0x03, 0xe8, 0x49, 0x40, 0x93, 0x6c, 0x7d, 0x09, 0x15, 0xdc,
            0x8c, 0xad,
        ];

        let instance_id = store
            .observe_boss_hp_guid(10.0, handle, 1000.0, "boss_hp".to_owned())
            .unwrap();

        let instance = store.instance(&instance_id).unwrap();
        assert_eq!(instance.target_name, "塞润尼缇");
        assert!(instance.aliases.iter().any(|alias| {
            alias.kind == TargetAliasKind::BossHpGuid
                && alias.value == "c55c885903e84940936c7d0915dc8cad"
        }));
    }

    #[test]
    fn resolves_hit_by_registered_alias_context() {
        let resources = ResourceIndex::load_default();
        let mut store = TargetInstanceStore::default();
        store.observe_paths(
            1.0,
            &[path("mon_01_BP")],
            &[identity(
                "mon_01_BP",
                NetIdentityCandidateKind::IrisNetRefHandle32,
                "0x81a599e2",
            )],
            &resources,
        );
        let mut hit = hit();
        hit.target_context.push("iris_ref32=0x81a599e2".to_owned());

        let resolution = store.resolve_hit(&hit).unwrap();

        assert_eq!(resolution.instance_id, "iris_ref32:0x81a599e2");
        assert_eq!(resolution.target_name, "低语种");
    }
}
