use std::collections::{HashMap, VecDeque};

use crate::resource_index::ResourceIndex;
use crate::ue_bitstream::PathCandidate;

const MAX_OBJECTS: usize = 512;
const MAX_EVIDENCE_PER_OBJECT: usize = 8;
const MAX_HP_HISTORY_PER_OBJECT: usize = 32;
const OBJECT_TTL_SECONDS: f64 = 20.0;
const ATTRIBUTE_PATH_LINK_WINDOW_SECONDS: f64 = 10.0;
const GAMEPLAY_EFFECT_PATH_LINK_WINDOW_SECONDS: f64 = 10.0;
const PATH_ONLY_DAMAGE_WINDOW_SECONDS: f64 = 6.0;
const HP_HISTORY_DAMAGE_WINDOW_SECONDS: f64 = 1.0;
const DEAD_OBJECT_DAMAGE_GRACE_SECONDS: f64 = 0.25;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[allow(dead_code)]
pub enum ObjectHandleKind {
    RuntimeInstance,
    NetGuidCandidate,
    NetRefHandleCandidate,
    AttributeGuid,
    PathOnly,
    Unknown,
}

impl ObjectHandleKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::RuntimeInstance => "RuntimeInstance",
            Self::NetGuidCandidate => "NetGuidCandidate",
            Self::NetRefHandleCandidate => "NetRefHandleCandidate",
            Self::AttributeGuid => "AttributeGuid",
            Self::PathOnly => "PathOnly",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct HpObservation {
    pub timestamp: f64,
    pub current: f64,
    pub max: Option<f64>,
    pub evidence: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ObjectDescriptor {
    pub handle_kind: ObjectHandleKind,
    pub handle: String,
    pub class_path: Option<String>,
    pub object_path: Option<String>,
    pub display_name: Option<String>,
    pub table_resolved_name: bool,
    pub owner_handle: Option<String>,
    pub actor_handle: Option<String>,
    pub component_handle: Option<String>,
    pub hp_current: Option<f64>,
    pub hp_max: Option<f64>,
    pub dead_at: Option<f64>,
    pub first_seen_at: f64,
    pub last_seen_at: f64,
    pub evidence: Vec<String>,
    pub confidence: f32,
    pub hp_history: VecDeque<HpObservation>,
}

#[derive(Clone, Debug, Default)]
pub struct ObjectStateStore {
    objects: HashMap<String, ObjectDescriptor>,
}

impl ObjectStateStore {
    pub fn observe_path_candidate(
        &mut self,
        timestamp: f64,
        candidate: &PathCandidate,
        resources: &ResourceIndex,
    ) -> String {
        let key = object_key(&ObjectHandleKind::PathOnly, &candidate.value);
        let raw_is_ignored = is_ignored_non_target_path(&candidate.value);
        let raw_has_resolved_name =
            !raw_is_ignored && resources.resolved_name_for_path(&candidate.value).is_some();
        let target_path = if raw_has_resolved_name {
            candidate.value.clone()
        } else {
            resources
                .canonical_target_path_for_path(&candidate.value)
                .unwrap_or_else(|| candidate.value.clone())
        };
        let canonical_target_alias = target_path != candidate.value
            && !is_ignored_non_target_path(&target_path)
            && resources.resolved_name_for_path(&target_path).is_some();
        let has_resolved_name = !is_ignored_non_target_path(&target_path)
            && (!raw_is_ignored || canonical_target_alias)
            && (resources.resolved_name_for_path(&target_path).is_some() || raw_has_resolved_name);
        let display_name = resources
            .display_name_for_path(&target_path)
            .or_else(|| resources.display_name_for_path(&candidate.value));
        let confidence =
            path_candidate_confidence(candidate.score, &target_path, has_resolved_name);
        let descriptor = self
            .objects
            .entry(key.clone())
            .or_insert_with(|| ObjectDescriptor {
                handle_kind: ObjectHandleKind::PathOnly,
                handle: candidate.value.clone(),
                class_path: Some(target_path.clone()).filter(|value| value.contains("/Game/")),
                object_path: Some(target_path.clone()),
                display_name: display_name.clone(),
                table_resolved_name: has_resolved_name,
                owner_handle: None,
                actor_handle: None,
                component_handle: None,
                hp_current: None,
                hp_max: None,
                dead_at: None,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                evidence: Vec::new(),
                confidence,
                hp_history: VecDeque::new(),
            });
        descriptor.last_seen_at = timestamp;
        descriptor.confidence = descriptor.confidence.max(confidence);
        descriptor.object_path = Some(target_path.clone());
        descriptor.table_resolved_name |= has_resolved_name;
        if target_path.contains("/Game/") {
            descriptor.class_path = Some(target_path.clone());
        }
        descriptor.display_name = descriptor.display_name.clone().or(display_name);
        push_unique_evidence(
            &mut descriptor.evidence,
            format!(
                "path_candidate:{}@{}:{}",
                candidate.value, candidate.byte_offset, candidate.bit_shift
            ),
        );
        self.link_path_to_single_hp_handle(timestamp, &target_path, resources);
        self.cleanup(timestamp);
        key
    }

    pub fn observe_hp_guid_update(
        &mut self,
        timestamp: f64,
        guid: [u8; 16],
        current_hp: f64,
        max_hp: Option<f64>,
        evidence: String,
    ) -> String {
        let handle = hex::encode(guid);
        self.observe_hp_update(
            timestamp,
            ObjectHandleKind::AttributeGuid,
            handle,
            current_hp,
            max_hp,
            evidence,
            0.65,
            0.70,
        )
    }

    pub fn observe_net_target_hp_update(
        &mut self,
        timestamp: f64,
        source: &str,
        token: &[u8],
        current_hp: f64,
        max_hp: Option<f64>,
        evidence: String,
    ) -> String {
        let handle = format!("{source}:{}", hex::encode(token));
        self.observe_hp_update(
            timestamp,
            ObjectHandleKind::NetRefHandleCandidate,
            handle,
            current_hp,
            max_hp,
            evidence,
            0.45,
            0.60,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_hp_update(
        &mut self,
        timestamp: f64,
        handle_kind: ObjectHandleKind,
        handle: String,
        current_hp: f64,
        max_hp: Option<f64>,
        evidence: String,
        initial_confidence: f32,
        observed_confidence: f32,
    ) -> String {
        let key = object_key(&handle_kind, &handle);
        let link_handle_kind = match handle_kind {
            ObjectHandleKind::AttributeGuid | ObjectHandleKind::NetRefHandleCandidate => {
                Some(handle_kind.clone())
            }
            _ => None,
        };
        let descriptor = self
            .objects
            .entry(key.clone())
            .or_insert_with(|| ObjectDescriptor {
                handle_kind: handle_kind.clone(),
                handle: handle.clone(),
                class_path: None,
                object_path: None,
                display_name: None,
                table_resolved_name: false,
                owner_handle: None,
                actor_handle: None,
                component_handle: None,
                hp_current: None,
                hp_max: max_hp,
                dead_at: None,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                evidence: Vec::new(),
                confidence: initial_confidence,
                hp_history: VecDeque::new(),
            });
        let mut reset_for_new_encounter = false;
        if is_new_hp_encounter(descriptor, timestamp, current_hp) {
            reset_for_new_encounter = true;
            descriptor.class_path = None;
            descriptor.object_path = None;
            descriptor.display_name = None;
            descriptor.table_resolved_name = false;
            descriptor.hp_max = max_hp;
            descriptor.dead_at = None;
            descriptor.first_seen_at = timestamp;
            descriptor.confidence = initial_confidence;
            descriptor.hp_history.clear();
            descriptor.evidence.clear();
            push_unique_evidence(
                &mut descriptor.evidence,
                format!("hp_encounter_reset:{current_hp:.0}"),
            );
        }
        descriptor.last_seen_at = timestamp;
        descriptor.hp_current = Some(current_hp);
        descriptor.hp_max = descriptor.hp_max.or(max_hp);
        descriptor.confidence = descriptor.confidence.max(observed_confidence);
        descriptor.hp_history.push_back(HpObservation {
            timestamp,
            current: current_hp,
            max: max_hp,
            evidence: evidence.clone(),
        });
        while descriptor.hp_history.len() > MAX_HP_HISTORY_PER_OBJECT {
            descriptor.hp_history.pop_front();
        }
        push_unique_evidence(&mut descriptor.evidence, evidence);
        let dead_path_to_clear = if current_hp <= 1.0 {
            descriptor.dead_at = Some(timestamp);
            descriptor.object_path.clone()
        } else {
            descriptor.dead_at = None;
            None
        };
        if let Some(dead_path) = dead_path_to_clear.as_deref() {
            self.remove_path_only_objects_for_path(dead_path);
        }
        if let Some(link_handle_kind) = link_handle_kind
            && !reset_for_new_encounter
            && current_hp > 1.0
        {
            self.link_hp_handle_to_best_path(&key, timestamp, link_handle_kind);
        }
        self.cleanup(timestamp);
        key
    }

    #[allow(dead_code)]
    pub fn observe_possible_handle(
        &mut self,
        timestamp: f64,
        handle_kind: ObjectHandleKind,
        handle: String,
        evidence: String,
    ) -> String {
        let key = object_key(&handle_kind, &handle);
        let descriptor = self
            .objects
            .entry(key.clone())
            .or_insert_with(|| ObjectDescriptor {
                handle_kind,
                handle,
                class_path: None,
                object_path: None,
                display_name: None,
                table_resolved_name: false,
                owner_handle: None,
                actor_handle: None,
                component_handle: None,
                hp_current: None,
                hp_max: None,
                dead_at: None,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                evidence: Vec::new(),
                confidence: 0.25,
                hp_history: VecDeque::new(),
            });
        descriptor.last_seen_at = timestamp;
        push_unique_evidence(&mut descriptor.evidence, evidence);
        self.cleanup(timestamp);
        key
    }

    #[allow(clippy::too_many_arguments)]
    pub fn observe_path_handle_candidate(
        &mut self,
        timestamp: f64,
        handle_kind: ObjectHandleKind,
        handle: String,
        path: &str,
        resources: &ResourceIndex,
        evidence: String,
        score: u16,
    ) -> String {
        let key = object_key(&handle_kind, &handle);
        let confidence = (score as f32 / 255.0).clamp(0.25, 0.55);
        let has_resolved_name = resources.resolved_name_for_path(path).is_some();
        let display_name = resources.display_name_for_path(path);
        let descriptor = self
            .objects
            .entry(key.clone())
            .or_insert_with(|| ObjectDescriptor {
                handle_kind,
                handle,
                class_path: None,
                object_path: None,
                display_name: None,
                table_resolved_name: false,
                owner_handle: None,
                actor_handle: None,
                component_handle: None,
                hp_current: None,
                hp_max: None,
                dead_at: None,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                evidence: Vec::new(),
                confidence,
                hp_history: VecDeque::new(),
            });
        descriptor.last_seen_at = timestamp;
        descriptor.confidence = descriptor.confidence.max(confidence);
        match descriptor.object_path.as_deref() {
            Some(existing) if existing != path => push_unique_evidence(
                &mut descriptor.evidence,
                format!("conflicting_path_anchor:{path}"),
            ),
            _ => {
                descriptor.object_path = Some(path.to_owned());
                if path.contains("/Game/") {
                    descriptor.class_path = Some(path.to_owned());
                }
                descriptor.display_name = descriptor.display_name.clone().or(display_name);
                descriptor.table_resolved_name |= has_resolved_name;
            }
        }
        push_unique_evidence(&mut descriptor.evidence, evidence);
        self.cleanup(timestamp);
        key
    }

    pub fn objects_near_time(&self, timestamp: f64, window_seconds: f64) -> Vec<&ObjectDescriptor> {
        self.objects
            .values()
            .filter(|object| (timestamp - object.last_seen_at).abs() <= window_seconds)
            .collect()
    }

    pub fn path_recently_died(&self, path: &str, timestamp: f64, window_seconds: f64) -> bool {
        self.objects
            .values()
            .filter(|object| object.handle_kind != ObjectHandleKind::PathOnly)
            .filter(|object| {
                object
                    .object_path
                    .as_deref()
                    .or(object.class_path.as_deref())
                    .is_some_and(|object_path| object_path.eq_ignore_ascii_case(path))
            })
            .any(|object| object_recently_died(object, timestamp, window_seconds))
    }

    pub fn candidates_for_damage(&self, timestamp: f64) -> Vec<&ObjectDescriptor> {
        self.objects
            .values()
            .filter(|object| object_is_near_damage(object, timestamp))
            .filter(|object| {
                object.hp_current.is_some()
                    || object
                        .object_path
                        .as_deref()
                        .or(object.class_path.as_deref())
                        .is_some_and(|path| {
                            (object.table_resolved_name && !is_ignored_non_target_path(path))
                                || is_targetish_path(path)
                        })
            })
            .collect()
    }

    pub fn link_unique_active_boss_hp_handle_to_path(
        &mut self,
        timestamp: f64,
        path: &str,
        resources: &ResourceIndex,
        evidence: String,
    ) -> bool {
        self.link_unique_active_boss_hp_handle_to_path_inner(
            timestamp, path, resources, evidence, false,
        )
    }

    fn link_unique_active_boss_hp_handle_to_path_inner(
        &mut self,
        timestamp: f64,
        path: &str,
        resources: &ResourceIndex,
        evidence: String,
        allow_override: bool,
    ) -> bool {
        if is_ignored_non_target_path(path) || resources.resolved_name_for_path(path).is_none() {
            return false;
        }
        let active_keys = self
            .objects
            .iter()
            .filter(|(_, object)| object.handle_kind == ObjectHandleKind::AttributeGuid)
            .filter(|(_, object)| object.hp_current.is_some_and(|hp| hp > 1.0))
            .filter(|(_, object)| object.dead_at.is_none())
            .filter(|(_, object)| {
                (timestamp - object.last_seen_at).abs() <= GAMEPLAY_EFFECT_PATH_LINK_WINDOW_SECONDS
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if active_keys.len() != 1 {
            return false;
        }
        let display_name = resources.display_name_for_path(path);
        let table_resolved_name = resources.resolved_name_for_path(path).is_some();
        let Some(object) = self.objects.get_mut(&active_keys[0]) else {
            return false;
        };
        let before_path = object.object_path.clone();
        let before_name = object.display_name.clone();
        push_unique_evidence(&mut object.evidence, evidence);
        apply_path_link(
            object,
            path,
            display_name,
            table_resolved_name,
            allow_override,
        );
        object.object_path != before_path || object.display_name != before_name
    }

    fn link_path_to_single_hp_handle(
        &mut self,
        timestamp: f64,
        path: &str,
        resources: &ResourceIndex,
    ) {
        if !is_targetish_path(path) {
            return;
        }
        let strong_paths =
            self.strong_targetish_paths_near(timestamp, ATTRIBUTE_PATH_LINK_WINDOW_SECONDS);
        if strong_paths.is_empty() {
            return;
        }
        if strong_paths.len() > 1 {
            for object in self
                .objects
                .values_mut()
                .filter(|object| is_linkable_hp_handle_kind(&object.handle_kind))
                .filter(|object| {
                    (timestamp - object.last_seen_at).abs() <= ATTRIBUTE_PATH_LINK_WINDOW_SECONDS
                })
            {
                mark_ambiguous_path_link(object, strong_paths.len(), &strong_paths);
            }
            return;
        }
        let linkable_keys = self
            .objects
            .iter()
            .filter(|(_, object)| is_linkable_hp_handle_kind(&object.handle_kind))
            .filter(|(_, object)| {
                (timestamp - object.last_seen_at).abs() <= ATTRIBUTE_PATH_LINK_WINDOW_SECONDS
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if linkable_keys.len() != 1 {
            return;
        }
        let linked_path = strong_paths[0].clone();
        let display_name = resources.display_name_for_path(&linked_path);
        let table_resolved_name = resources.resolved_name_for_path(&linked_path).is_some();
        if let Some(object) = self.objects.get_mut(&linkable_keys[0]) {
            apply_path_link(
                object,
                &linked_path,
                display_name,
                table_resolved_name,
                false,
            );
        }
    }

    fn link_hp_handle_to_best_path(
        &mut self,
        object_key: &str,
        timestamp: f64,
        handle_kind: ObjectHandleKind,
    ) {
        let strong_paths =
            self.strong_targetish_paths_near(timestamp, ATTRIBUTE_PATH_LINK_WINDOW_SECONDS);
        if strong_paths.len() > 1 {
            if let Some(object) = self.objects.get_mut(object_key) {
                mark_ambiguous_path_link(object, strong_paths.len(), &strong_paths);
            }
            return;
        }
        let Some(path) = strong_paths.first() else {
            return;
        };
        let linkable_keys = self
            .objects
            .iter()
            .filter(|(_, object)| object.handle_kind == handle_kind)
            .filter(|(_, object)| {
                (timestamp - object.last_seen_at).abs() <= ATTRIBUTE_PATH_LINK_WINDOW_SECONDS
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if linkable_keys.len() != 1 || linkable_keys[0] != object_key {
            return;
        }
        let (display_name, table_resolved_name) = self
            .objects
            .values()
            .find(|object| object.object_path.as_deref() == Some(path.as_str()))
            .map(|object| (object.display_name.clone(), object.table_resolved_name))
            .unwrap_or((None, false));
        if let Some(object) = self.objects.get_mut(object_key) {
            apply_path_link(object, path, display_name, table_resolved_name, false);
        }
    }

    fn strong_targetish_paths_near(&self, timestamp: f64, window_seconds: f64) -> Vec<String> {
        let path_objects = self
            .objects
            .values()
            .filter(|object| object.handle_kind == ObjectHandleKind::PathOnly)
            .filter(|object| (timestamp - object.last_seen_at).abs() <= window_seconds)
            .filter_map(|object| {
                let path = object.object_path.as_deref()?;
                strong_targetish_path(object, path).then_some(object)
            })
            .collect::<Vec<_>>();
        dominant_targetish_paths(path_objects)
    }

    pub fn cleanup(&mut self, timestamp: f64) {
        self.objects
            .retain(|_, object| timestamp - object.last_seen_at <= OBJECT_TTL_SECONDS);
        if self.objects.len() <= MAX_OBJECTS {
            return;
        }
        let mut keys = self
            .objects
            .iter()
            .map(|(key, object)| (key.clone(), object.last_seen_at))
            .collect::<Vec<_>>();
        keys.sort_by(|left, right| left.1.total_cmp(&right.1));
        let remove_count = self.objects.len() - MAX_OBJECTS;
        for (key, _) in keys.into_iter().take(remove_count) {
            self.objects.remove(&key);
        }
    }

    pub fn clear_handle_identity(
        &mut self,
        timestamp: f64,
        handle_kind: ObjectHandleKind,
        handle: &str,
        evidence: String,
    ) -> bool {
        let key = object_key(&handle_kind, handle);
        let Some(object) = self.objects.get_mut(&key) else {
            return false;
        };
        let old_path = object.object_path.clone();
        object.class_path = None;
        object.object_path = None;
        object.display_name = None;
        object.table_resolved_name = false;
        object.confidence = object.confidence.min(0.35);
        object.dead_at = None;
        object.last_seen_at = timestamp;
        push_unique_evidence(&mut object.evidence, evidence);
        if let Some(old_path) = old_path {
            self.remove_path_only_objects_for_path(&old_path);
        }
        true
    }

    fn remove_path_only_objects_for_path(&mut self, path: &str) {
        self.objects.retain(|_, object| {
            object.handle_kind != ObjectHandleKind::PathOnly
                || !object
                    .object_path
                    .as_deref()
                    .is_some_and(|object_path| object_path.eq_ignore_ascii_case(path))
        });
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.objects.len()
    }
}

#[derive(Clone, Debug)]
struct TargetPathGroup {
    path: String,
    weight: usize,
    representative_rank: i32,
    last_seen_at: f64,
}

fn dominant_targetish_paths(path_objects: Vec<&ObjectDescriptor>) -> Vec<String> {
    let mut groups = HashMap::<String, TargetPathGroup>::new();
    for object in path_objects {
        let Some(path) = object.object_path.as_deref() else {
            continue;
        };
        let group_key = target_group_key(path).unwrap_or_else(|| path.to_ascii_lowercase());
        let weight = path_observation_weight(object);
        let rank = target_path_representative_rank(path);
        groups
            .entry(group_key)
            .and_modify(|group| {
                group.weight += weight;
                if rank > group.representative_rank
                    || (rank == group.representative_rank
                        && object.last_seen_at > group.last_seen_at)
                {
                    group.path = path.to_owned();
                    group.representative_rank = rank;
                    group.last_seen_at = object.last_seen_at;
                }
            })
            .or_insert_with(|| TargetPathGroup {
                path: path.to_owned(),
                weight,
                representative_rank: rank,
                last_seen_at: object.last_seen_at,
            });
    }

    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        right
            .weight
            .cmp(&left.weight)
            .then_with(|| right.representative_rank.cmp(&left.representative_rank))
            .then_with(|| right.last_seen_at.total_cmp(&left.last_seen_at))
            .then_with(|| left.path.cmp(&right.path))
    });

    if groups.is_empty() {
        return Vec::new();
    }
    if groups.len() == 1 {
        return vec![groups[0].path.clone()];
    }
    if groups[0].weight >= 2 && groups[0].weight > groups[1].weight {
        return vec![groups[0].path.clone()];
    }

    let mut paths = groups
        .into_iter()
        .map(|group| group.path)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn path_observation_weight(object: &ObjectDescriptor) -> usize {
    object
        .evidence
        .iter()
        .filter(|entry| entry.starts_with("path_candidate:"))
        .count()
        .max(1)
}

fn target_path_representative_rank(path: &str) -> i32 {
    let lower = path.to_ascii_lowercase();
    let mut rank = 0;
    if lower.starts_with("worldboss_boss") {
        rank += 40;
    }
    if lower.contains("/monster/") {
        rank += 30;
    }
    if lower.contains("_bp") {
        rank += 20;
    }
    if lower.contains("worldboss") {
        rank += 20;
    }
    for weak_marker in ["entrance", "exit", "summon", "spawn", "drop"] {
        if lower.contains(weak_marker) {
            rank -= 15;
        }
    }
    rank
}

fn target_group_key(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    for (prefix, marker) in [
        ("boss", "worldboss_boss"),
        ("boss", "worldboss_"),
        ("boss", "weeklyclone_boss"),
        ("boss", "boss_"),
        ("mon", "mon_"),
    ] {
        if let Some(number) = number_after_marker(&lower, marker) {
            return Some(format!("{prefix}_{number}"));
        }
    }
    let basename = lower
        .rsplit('/')
        .next()
        .unwrap_or(&lower)
        .rsplit('.')
        .next()
        .unwrap_or(&lower)
        .trim_end_matches("_c")
        .strip_prefix("default__")
        .unwrap_or_else(|| {
            lower
                .rsplit('/')
                .next()
                .unwrap_or(&lower)
                .rsplit('.')
                .next()
                .unwrap_or(&lower)
                .trim_end_matches("_c")
        });
    for (prefix, marker) in [("boss", "boss"), ("mon", "mon")] {
        if let Some(number) = number_after_prefix(basename, marker) {
            return Some(format!("{prefix}_{number}"));
        }
    }
    None
}

fn number_after_marker(value: &str, marker: &str) -> Option<u32> {
    let mut search_start = 0;
    while let Some(relative_index) = value[search_start..].find(marker) {
        let digit_start = search_start + relative_index + marker.len();
        let digits = value[digit_start..]
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        if !digits.is_empty() {
            return digits.parse::<u32>().ok();
        }
        search_start = digit_start;
        if search_start >= value.len() {
            break;
        }
    }
    None
}

fn number_after_prefix(value: &str, prefix: &str) -> Option<u32> {
    let digit_start = value.strip_prefix(prefix)?;
    let digits = digit_start
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

fn is_new_hp_encounter(object: &ObjectDescriptor, timestamp: f64, current_hp: f64) -> bool {
    let Some(previous_hp) = object.hp_current else {
        return false;
    };
    if previous_hp <= 1.0 && current_hp > 1.0 {
        return true;
    }
    if object.dead_at.is_some() && current_hp > 1.0 {
        return true;
    }
    if timestamp - object.last_seen_at < 2.0 {
        return false;
    }
    current_hp - previous_hp > 500_000.0 && current_hp > previous_hp * 3.0
}

fn apply_path_link(
    attribute: &mut ObjectDescriptor,
    path: &str,
    display_name: Option<String>,
    table_resolved_name: bool,
    allow_override: bool,
) {
    if let Some(existing_path) = attribute.object_path.as_deref()
        && existing_path != path
    {
        push_unique_evidence(
            &mut attribute.evidence,
            format!("conflicting_path_link:{path}"),
        );
        if !allow_override {
            return;
        }
    }
    attribute.object_path = Some(path.to_owned());
    attribute.table_resolved_name |= table_resolved_name;
    if path.contains("/Game/") {
        attribute.class_path = Some(path.to_owned());
    }
    if allow_override {
        attribute.display_name = display_name.or_else(|| attribute.display_name.clone());
    } else {
        attribute.display_name = attribute.display_name.clone().or(display_name);
    }
    attribute.confidence = attribute.confidence.max(0.80);
    push_unique_evidence(&mut attribute.evidence, format!("linked_path:{path}"));
}

fn is_linkable_hp_handle_kind(handle_kind: &ObjectHandleKind) -> bool {
    matches!(
        handle_kind,
        ObjectHandleKind::AttributeGuid | ObjectHandleKind::NetRefHandleCandidate
    )
}

fn path_candidate_confidence(score: u16, target_path: &str, has_resolved_name: bool) -> f32 {
    let base = (score as f32 / 255.0).clamp(0.1, 0.45);
    if has_resolved_name || is_targetish_path(target_path) {
        base.max(0.75)
    } else {
        base
    }
}

fn mark_ambiguous_path_link(attribute: &mut ObjectDescriptor, count: usize, paths: &[String]) {
    if attribute.object_path.is_none() {
        attribute.class_path = None;
        attribute.display_name = None;
        attribute.table_resolved_name = false;
    }
    push_unique_evidence(
        &mut attribute.evidence,
        format!("ambiguous_path_link:{count}"),
    );
    for path in paths.iter().take(3) {
        push_unique_evidence(&mut attribute.evidence, format!("ambiguous_path:{path}"));
    }
}

fn strong_targetish_path(object: &ObjectDescriptor, path: &str) -> bool {
    is_targetish_path(path)
        && (path.starts_with("/Game/") || is_world_boss_path(path) || object.confidence >= 0.70)
}

fn object_key(kind: &ObjectHandleKind, handle: &str) -> String {
    format!("{}:{handle}", kind.label())
}

fn push_unique_evidence(evidence: &mut Vec<String>, value: String) {
    if evidence.iter().any(|item| item == &value) {
        return;
    }
    evidence.push(value);
    if evidence.len() > MAX_EVIDENCE_PER_OBJECT {
        evidence.remove(0);
    }
}

fn damage_candidate_window(object: &ObjectDescriptor) -> f64 {
    if object.hp_current.is_some() {
        return 1.0;
    }
    let target_path = object
        .object_path
        .as_deref()
        .or(object.class_path.as_deref());
    if object.table_resolved_name || target_path.is_some_and(is_precise_target_path) {
        PATH_ONLY_DAMAGE_WINDOW_SECONDS
    } else {
        1.0
    }
}

fn object_is_near_damage(object: &ObjectDescriptor, timestamp: f64) -> bool {
    if object
        .dead_at
        .is_some_and(|dead_at| timestamp > dead_at + DEAD_OBJECT_DAMAGE_GRACE_SECONDS)
    {
        return false;
    }
    if (timestamp - object.last_seen_at).abs() <= damage_candidate_window(object) {
        return true;
    }
    if object_has_named_hp_target(object)
        && timestamp + HP_HISTORY_DAMAGE_WINDOW_SECONDS >= object.first_seen_at
        && timestamp <= object.last_seen_at + ATTRIBUTE_PATH_LINK_WINDOW_SECONDS
    {
        return true;
    }
    object.hp_history.iter().any(|observation| {
        (timestamp - observation.timestamp).abs() <= HP_HISTORY_DAMAGE_WINDOW_SECONDS
    })
}

fn object_recently_died(object: &ObjectDescriptor, timestamp: f64, window_seconds: f64) -> bool {
    object
        .dead_at
        .is_some_and(|dead_at| (timestamp - dead_at).abs() <= window_seconds)
        || object.hp_history.iter().any(|observation| {
            observation.current <= 1.0
                && (timestamp - observation.timestamp).abs() <= window_seconds
        })
}

fn object_has_named_hp_target(object: &ObjectDescriptor) -> bool {
    object.hp_current.is_some()
        && object.display_name.is_some()
        && object.table_resolved_name
        && object
            .object_path
            .as_deref()
            .or(object.class_path.as_deref())
            .is_some_and(|path| object.table_resolved_name || is_targetish_path(path))
}

pub fn is_targetish_path(value: &str) -> bool {
    if is_ignored_non_target_path(value) {
        return false;
    }
    if is_precise_target_path(value) || is_world_boss_path(value) {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    ["enemy", "npc", "htcharacter"]
        .iter()
        .any(|needle| lower.contains(needle))
}

pub fn is_precise_target_path(value: &str) -> bool {
    if is_ignored_non_target_path(value) || is_world_boss_path(value) {
        return false;
    }
    target_group_key(value).is_some()
}

pub fn is_ignored_non_target_path(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let basename = lower
        .rsplit('/')
        .next()
        .unwrap_or(&lower)
        .rsplit('.')
        .next()
        .unwrap_or(&lower)
        .trim_end_matches("_c")
        .strip_prefix("default__")
        .unwrap_or_else(|| {
            lower
                .rsplit('/')
                .next()
                .unwrap_or(&lower)
                .rsplit('.')
                .next()
                .unwrap_or(&lower)
                .trim_end_matches("_c")
        });
    basename.starts_with("buff_")
        || basename.starts_with("ge_")
        || basename.starts_with("ga_")
        || basename.contains("steal_montage")
        || basename.ends_with("_montage")
        || basename.starts_with("drop")
        || basename.starts_with("dropbox")
        || lower.contains("drop_mon_")
        || lower.contains("/drop/")
        || lower.contains("/dropbox/")
        || basename.contains("lockhp")
        || lower.contains("/monsterbase/")
        || lower.contains("/abilities/")
        || lower.contains("/ability/")
        || lower.contains("/buff/")
        || lower.contains("/effect/")
        || lower.contains("/cooldown/")
        || lower.contains("/passiveeffect/")
}

fn is_world_boss_path(value: &str) -> bool {
    value.to_ascii_lowercase().contains("worldboss")
}
