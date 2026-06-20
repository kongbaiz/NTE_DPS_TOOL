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
const INSTANCE_DEAD_HP_THRESHOLD: f64 = 1.0;
const HP_MATCH_TOLERANCE_ABSOLUTE: f64 = 2.0;
const HP_MATCH_TOLERANCE_RATIO: f64 = 0.002;
const HIT_VECTOR_INSTANCE_WINDOW_SECONDS: f64 = 20.0;
const HIT_VECTOR_INSTANCE_DISTANCE: f64 = 300.0;
const UNKNOWN_RUNTIME_TARGET_PATH: &str = "runtime://unknown_target";
const MAX_HIT_VECTOR_PLACEHOLDER_HP: f64 = 500_000.0;

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
    pub hit_xyz: Option<[f64; 3]>,
    pub observed_max_hp: Option<f64>,
    pub placeholder: bool,
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
                if instance.placeholder {
                    return Some(instance_resolution(
                        instance,
                        TargetConfidence::Probable,
                        70,
                        "runtime_placeholder_hit_vector".to_owned(),
                    ));
                }
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
                "runtime_hp_timeline_unique".to_owned(),
            ));
        }
        None
    }

    pub fn observe_hit_vector_hit(&mut self, hit: &mut Hit) -> Option<String> {
        if hit.direction != "outgoing"
            || hit.target_name.is_some()
            || hit.target_id.is_some()
            || hit.target_max_hp > MAX_HIT_VECTOR_PLACEHOLDER_HP
            || has_strong_target_context(&hit.target_context)
        {
            return None;
        }
        let token = target_context_value(&hit.target_context, "hit_target_vector_token")?;
        let xyz =
            target_context_value(&hit.target_context, "hit_target_xyz").and_then(parse_xyz)?;
        let alias = TargetAlias::new(TargetAliasKind::HitVectorToken, token);
        let instance_id = self
            .alias_index
            .get(&alias.key())
            .cloned()
            .or_else(|| self.unique_hit_vector_match(hit, xyz))
            .unwrap_or_else(|| {
                self.create_placeholder_instance(hit.timestamp, xyz, hit.target_max_hp)
            });
        let current_id = self.add_alias_and_maybe_rename(&instance_id, alias);
        let instance = self.instances.get_mut(&current_id)?;
        instance.last_seen_at = hit.timestamp;
        instance.hit_xyz = Some(xyz);
        instance.observed_max_hp = Some(hit.target_max_hp);
        instance.hp_current = Some(hit.target_hp_after);
        instance.state = if hit.target_hp_after <= INSTANCE_DEAD_HP_THRESHOLD {
            RuntimeTargetState::Dead
        } else {
            RuntimeTargetState::Active
        };
        instance.hp_history.push_back(RuntimeTargetHpObservation {
            timestamp: hit.timestamp,
            current: hit.target_hp_after,
            evidence: "hit_vector_hp_timeline".to_owned(),
        });
        while instance.hp_history.len() > MAX_HP_HISTORY_PER_INSTANCE {
            instance.hp_history.pop_front();
        }
        let confidence = if instance.hp_history.len() >= 2 {
            "probable"
        } else {
            "possible"
        };
        push_unique_context(
            &mut hit.target_context,
            format!("runtime_target_instance={}", instance.instance_id),
        );
        push_unique_context(
            &mut hit.target_context,
            format!("target_instance_confidence={confidence}"),
        );
        push_unique_context(
            &mut hit.target_context,
            "target_instance_reason=hit_vector_hp_timeline".to_owned(),
        );
        push_unique_context(
            &mut hit.target_context,
            format!("target_max_hp_observation={:.0}", hit.target_max_hp),
        );
        Some(instance.instance_id.clone())
    }

    pub fn active_named_instance_count(&self, timestamp: f64) -> usize {
        self.active_named_instances(timestamp).count()
    }

    pub fn instance_for_alias(&self, alias: &TargetAlias) -> Option<&RuntimeTargetInstance> {
        self.alias_index
            .get(&alias.key())
            .and_then(|instance_id| self.instances.get(instance_id))
            .filter(|instance| instance.state == RuntimeTargetState::Active)
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
            if instance
                .canonical_path
                .eq_ignore_ascii_case(&canonical_path)
            {
                instance.target_name = target_name;
                instance.canonical_path = canonical_path;
            }
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
                hit_xyz: None,
                observed_max_hp: None,
                placeholder: false,
                hp_history: VecDeque::new(),
                state: RuntimeTargetState::Active,
            },
        );
        instance_id
    }

    fn create_placeholder_instance(
        &mut self,
        timestamp: f64,
        xyz: [f64; 3],
        observed_max_hp: f64,
    ) -> String {
        let spawn_seq = self
            .spawn_seq_by_path
            .entry(UNKNOWN_RUNTIME_TARGET_PATH.to_owned())
            .and_modify(|value| *value += 1)
            .or_insert(1);
        let instance_id = format!("runtime_unknown_target#{}", *spawn_seq);
        self.instances.insert(
            instance_id.clone(),
            RuntimeTargetInstance {
                instance_id: instance_id.clone(),
                canonical_path: UNKNOWN_RUNTIME_TARGET_PATH.to_owned(),
                target_name: format!("未知目标#{}", *spawn_seq),
                spawn_seq: *spawn_seq,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                aliases: BTreeSet::new(),
                hp_current: None,
                hit_xyz: Some(xyz),
                observed_max_hp: Some(observed_max_hp),
                placeholder: true,
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
        let (instance_id, dead_alias_keys) = {
            let instance = self.instances.get_mut(&current_id)?;
            instance.last_seen_at = timestamp;
            instance.hp_current = Some(current_hp);
            if current_hp <= INSTANCE_DEAD_HP_THRESHOLD {
                instance.state = RuntimeTargetState::Dead;
            } else {
                instance.state = RuntimeTargetState::Active;
            }
            instance.hp_history.push_back(RuntimeTargetHpObservation {
                timestamp,
                current: current_hp,
                evidence,
            });
            while instance.hp_history.len() > MAX_HP_HISTORY_PER_INSTANCE {
                instance.hp_history.pop_front();
            }
            let dead_alias_keys = if current_hp <= INSTANCE_DEAD_HP_THRESHOLD {
                instance
                    .aliases
                    .iter()
                    .map(TargetAlias::key)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            (instance.instance_id.clone(), dead_alias_keys)
        };
        for key in dead_alias_keys {
            self.alias_index.remove(&key);
        }
        (current_hp > INSTANCE_DEAD_HP_THRESHOLD).then_some(instance_id)
    }

    fn add_alias_and_maybe_rename(&mut self, instance_id: &str, alias: TargetAlias) -> String {
        let current_id = self.current_id_for(instance_id);
        let Some(instance) = self.instances.get_mut(&current_id) else {
            return current_id;
        };
        instance.aliases.insert(alias.clone());
        self.alias_index.insert(alias.key(), current_id.clone());
        if instance.placeholder && alias.kind == TargetAliasKind::HitVectorToken {
            return current_id;
        }
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
            .filter(|instance| instance.state == RuntimeTargetState::Active)
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

    fn unique_hit_vector_match(&self, hit: &Hit, xyz: [f64; 3]) -> Option<String> {
        let mut candidates = self
            .instances
            .values()
            .filter(|instance| instance.placeholder)
            .filter(|instance| instance.state == RuntimeTargetState::Active)
            .filter(|instance| {
                let age = hit.timestamp - instance.last_seen_at;
                (0.0..=HIT_VECTOR_INSTANCE_WINDOW_SECONDS).contains(&age)
            })
            .filter(|instance| {
                instance
                    .hit_xyz
                    .is_some_and(|existing| distance(existing, xyz) <= HIT_VECTOR_INSTANCE_DISTANCE)
            })
            .filter(|instance| placeholder_hp_matches(instance, hit))
            .map(|instance| (instance.instance_id.clone(), instance.last_seen_at))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.1.total_cmp(&left.1));
        (candidates.len() == 1).then(|| candidates.remove(0).0)
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
    if instance.state != RuntimeTargetState::Active {
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

fn target_context_value<'a>(context: &'a [String], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    context
        .iter()
        .find_map(|value| value.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "None")
}

fn parse_xyz(value: &str) -> Option<[f64; 3]> {
    let mut parts = value.split(',');
    let x = parts.next()?.trim().parse().ok()?;
    let y = parts.next()?.trim().parse().ok()?;
    let z = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some([x, y, z])
}

fn distance(left: [f64; 3], right: [f64; 3]) -> f64 {
    ((left[0] - right[0]).powi(2) + (left[1] - right[1]).powi(2) + (left[2] - right[2]).powi(2))
        .sqrt()
}

fn placeholder_hp_matches(instance: &RuntimeTargetInstance, hit: &Hit) -> bool {
    if let Some(current) = instance.hp_current
        && nearly_equal(
            current,
            hit.target_hp_before,
            hit.target_hp_before.max(current),
        )
    {
        return true;
    }
    instance.hp_history.is_empty()
}

fn has_strong_target_context(context: &[String]) -> bool {
    context.iter().any(|entry| {
        entry.starts_with("boss_hp_guid=")
            || entry.starts_with("target_handle_candidate=AttributeGuid:")
            || entry == "target_name_resolution=table_resolved"
            || entry.contains("direct_hp")
            || entry.contains("boss_hp_delta_match")
            || entry.contains("net_target_hp_delta_match")
            || entry.contains("hp_guid_timeline_match")
            || entry.contains("net_target_hp_timeline_match")
    })
}

fn push_unique_context(context: &mut Vec<String>, value: String) {
    if !context.iter().any(|entry| entry == &value) {
        context.push(value);
    }
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

    fn vector_hit(timestamp: f64, before: f64, after: f64, max_hp: f64, xyz: [f64; 3]) -> Hit {
        Hit {
            timestamp,
            char_id: 1,
            char_name: "tester".to_owned(),
            char_known: true,
            damage: (before - after).abs().max(1.0),
            byte_offset: timestamp as usize,
            bit_shift: 0,
            char_source: "packet".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: before,
            target_hp_after: after,
            target_max_hp: max_hp,
            target_hp_percent: if max_hp > 0.0 {
                after / max_hp * 100.0
            } else {
                0.0
            },
            target_id: None,
            target_name: None,
            target_context: vec![
                format!("hit_target_vector_token=token-{timestamp:.3}"),
                format!("hit_target_xyz={:.3},{:.3},{:.3}", xyz[0], xyz[1], xyz[2]),
            ],
            gameplay_effect_index: None,
            gameplay_effect_name: None,
            ability_name: None,
            damage_name: None,
            attack_type: None,
        }
    }

    #[test]
    fn active_alias_lock_does_not_rename_to_different_path() {
        let mut store = TargetInstanceStore::default();
        let alias = TargetAlias::new(TargetAliasKind::NetGuid32, "abcd");
        let first_id = store.observe_runtime_mapping(
            1.0,
            "/Game/Monster/Boss_001_BP.Boss_001_BP_C".to_owned(),
            "Target A".to_owned(),
            vec![alias.clone()],
        );
        let second_id = store.observe_runtime_mapping(
            1.5,
            "/Game/Monster/Boss_002_BP.Boss_002_BP_C".to_owned(),
            "Target B".to_owned(),
            vec![alias.clone()],
        );

        assert_eq!(first_id, second_id);
        let first = store.instance(&first_id).expect("first instance");
        assert_eq!(first.target_name, "Target A");
        assert_eq!(
            first.canonical_path,
            "/Game/Monster/Boss_001_BP.Boss_001_BP_C"
        );
    }

    #[test]
    fn same_name_same_path_keeps_distinct_generations_for_distinct_aliases() {
        let mut store = TargetInstanceStore::default();
        let first_id = store.observe_runtime_mapping(
            1.0,
            "/Game/Monster/Mon_001_BP.Mon_001_BP_C".to_owned(),
            "Same Target".to_owned(),
            vec![TargetAlias::new(TargetAliasKind::NetGuid32, "a1")],
        );
        let second_id = store.observe_runtime_mapping(
            2.0,
            "/Game/Monster/Mon_001_BP.Mon_001_BP_C".to_owned(),
            "Same Target".to_owned(),
            vec![TargetAlias::new(TargetAliasKind::NetGuid32, "a2")],
        );

        assert_ne!(first_id, second_id);
        assert_eq!(store.instance(&first_id).expect("first").spawn_seq, 1);
        assert_eq!(store.instance(&second_id).expect("second").spawn_seq, 2);
    }

    #[test]
    fn hit_vector_runtime_instances_do_not_merge_different_small_targets() {
        let mut store = TargetInstanceStore::default();
        let mut a1 = vector_hit(1.0, 29104.0, 25681.0, 29104.0, [0.0, 0.0, 0.0]);
        let a_id = store
            .observe_hit_vector_hit(&mut a1)
            .expect("first target instance");
        let mut a2 = vector_hit(2.0, 25681.0, 12712.0, 29104.0, [120.0, 20.0, 0.0]);
        let a2_id = store
            .observe_hit_vector_hit(&mut a2)
            .expect("same target instance");
        let mut b = vector_hit(2.5, 168096.0, 167637.0, 168096.0, [2000.0, 0.0, 0.0]);
        let b_id = store
            .observe_hit_vector_hit(&mut b)
            .expect("second target instance");

        assert_eq!(a_id, a2_id);
        assert_ne!(a_id, b_id);
        assert!(
            a2.target_context
                .iter()
                .any(|entry| entry == "target_instance_reason=hit_vector_hp_timeline")
        );
    }

    #[test]
    fn hit_vector_placeholder_does_not_override_existing_boss_target() {
        let mut store = TargetInstanceStore::default();
        let mut hit = vector_hit(1.0, 1_000_000.0, 900_000.0, 1_000_000.0, [0.0, 0.0, 0.0]);
        hit.target_id = Some("monster:boss_13#1".to_owned());
        hit.target_name = Some("斑蝶".to_owned());
        let result = store.observe_hit_vector_hit(&mut hit);

        assert_eq!(result, None);
        assert_eq!(hit.target_id.as_deref(), Some("monster:boss_13#1"));
        assert_eq!(hit.target_name.as_deref(), Some("斑蝶"));
        assert!(
            !hit.target_context
                .iter()
                .any(|entry| entry.starts_with("runtime_target_instance="))
        );
    }

    #[test]
    fn hit_vector_placeholder_requires_unresolved_outgoing_low_hp() {
        let mut store = TargetInstanceStore::default();
        let mut incoming = vector_hit(1.0, 10_000.0, 9_000.0, 10_000.0, [0.0, 0.0, 0.0]);
        incoming.direction = "incoming".to_owned();
        assert_eq!(store.observe_hit_vector_hit(&mut incoming), None);

        let mut high_hp = vector_hit(2.0, 900_000.0, 800_000.0, 900_000.0, [0.0, 0.0, 0.0]);
        assert_eq!(store.observe_hit_vector_hit(&mut high_hp), None);

        let mut resolved = vector_hit(3.0, 10_000.0, 9_000.0, 10_000.0, [0.0, 0.0, 0.0]);
        resolved.target_id = Some("AttributeGuid:abc".to_owned());
        assert_eq!(store.observe_hit_vector_hit(&mut resolved), None);
    }

    #[test]
    fn hit_vector_placeholder_rejects_strong_target_context() {
        let mut store = TargetInstanceStore::default();
        let mut hit = vector_hit(1.0, 10_000.0, 9_000.0, 10_000.0, [0.0, 0.0, 0.0]);
        hit.target_context
            .push("boss_hp_guid=00112233445566778899aabbccddeeff".to_owned());
        hit.target_context
            .push("reason=hp_guid_timeline_match:test".to_owned());

        assert_eq!(store.observe_hit_vector_hit(&mut hit), None);
        assert!(
            !hit.target_context
                .iter()
                .any(|entry| entry.starts_with("runtime_target_instance="))
        );
    }
}
