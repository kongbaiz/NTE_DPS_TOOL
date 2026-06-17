use std::collections::{HashMap, VecDeque};

use crate::resource_index::ResourceIndex;
use crate::ue_bitstream::PathCandidate;

const MAX_OBJECTS: usize = 512;
const MAX_EVIDENCE_PER_OBJECT: usize = 8;
const MAX_HP_HISTORY_PER_OBJECT: usize = 32;
const OBJECT_TTL_SECONDS: f64 = 20.0;
const ATTRIBUTE_PATH_LINK_WINDOW_SECONDS: f64 = 1.0;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[allow(dead_code)]
pub enum ObjectHandleKind {
    NetGuidCandidate,
    NetRefHandleCandidate,
    AttributeGuid,
    PathOnly,
    Unknown,
}

impl ObjectHandleKind {
    pub fn label(&self) -> &'static str {
        match self {
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
    pub owner_handle: Option<String>,
    pub actor_handle: Option<String>,
    pub component_handle: Option<String>,
    pub hp_current: Option<f64>,
    pub hp_max: Option<f64>,
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
        let descriptor = self.objects.entry(key.clone()).or_insert_with(|| {
            let display_name = resources.display_name_for_path(&candidate.value);
            ObjectDescriptor {
                handle_kind: ObjectHandleKind::PathOnly,
                handle: candidate.value.clone(),
                class_path: Some(candidate.value.clone()).filter(|value| value.contains("/Game/")),
                object_path: Some(candidate.value.clone()),
                display_name,
                owner_handle: None,
                actor_handle: None,
                component_handle: None,
                hp_current: None,
                hp_max: None,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                evidence: Vec::new(),
                confidence: (candidate.score as f32 / 255.0).clamp(0.1, 0.45),
                hp_history: VecDeque::new(),
            }
        });
        descriptor.last_seen_at = timestamp;
        descriptor.confidence = descriptor
            .confidence
            .max((candidate.score as f32 / 255.0).clamp(0.1, 0.45));
        push_unique_evidence(
            &mut descriptor.evidence,
            format!(
                "path_candidate:{}@{}:{}",
                candidate.value, candidate.byte_offset, candidate.bit_shift
            ),
        );
        self.link_path_to_single_attribute(timestamp, &candidate.value, resources);
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
        let key = object_key(&ObjectHandleKind::AttributeGuid, &handle);
        let descriptor = self
            .objects
            .entry(key.clone())
            .or_insert_with(|| ObjectDescriptor {
                handle_kind: ObjectHandleKind::AttributeGuid,
                handle: handle.clone(),
                class_path: None,
                object_path: None,
                display_name: None,
                owner_handle: None,
                actor_handle: None,
                component_handle: None,
                hp_current: None,
                hp_max: max_hp,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                evidence: Vec::new(),
                confidence: 0.65,
                hp_history: VecDeque::new(),
            });
        descriptor.last_seen_at = timestamp;
        descriptor.hp_current = Some(current_hp);
        descriptor.hp_max = descriptor.hp_max.or(max_hp);
        descriptor.confidence = descriptor.confidence.max(0.70);
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
        self.link_attribute_to_best_path(&key, timestamp);
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
                owner_handle: None,
                actor_handle: None,
                component_handle: None,
                hp_current: None,
                hp_max: None,
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

    pub fn objects_near_time(&self, timestamp: f64, window_seconds: f64) -> Vec<&ObjectDescriptor> {
        self.objects
            .values()
            .filter(|object| (timestamp - object.last_seen_at).abs() <= window_seconds)
            .collect()
    }

    pub fn candidates_for_damage(&self, timestamp: f64) -> Vec<&ObjectDescriptor> {
        self.objects_near_time(timestamp, 1.0)
            .into_iter()
            .filter(|object| {
                object.hp_current.is_some()
                    || object
                        .object_path
                        .as_deref()
                        .or(object.class_path.as_deref())
                        .is_some_and(is_targetish_path)
            })
            .collect()
    }

    fn link_path_to_single_attribute(
        &mut self,
        timestamp: f64,
        path: &str,
        resources: &ResourceIndex,
    ) {
        if !is_targetish_path(path) {
            return;
        }
        let strong_paths = self.strong_targetish_paths_near(timestamp);
        if strong_paths.is_empty() {
            return;
        }
        if strong_paths.len() > 1 {
            for attribute in self
                .objects
                .values_mut()
                .filter(|object| object.handle_kind == ObjectHandleKind::AttributeGuid)
                .filter(|object| {
                    (timestamp - object.last_seen_at).abs() <= ATTRIBUTE_PATH_LINK_WINDOW_SECONDS
                })
            {
                mark_ambiguous_path_link(attribute, strong_paths.len(), &strong_paths);
            }
            return;
        }
        let attribute_keys = self
            .objects
            .iter()
            .filter(|(_, object)| object.handle_kind == ObjectHandleKind::AttributeGuid)
            .filter(|(_, object)| {
                (timestamp - object.last_seen_at).abs() <= ATTRIBUTE_PATH_LINK_WINDOW_SECONDS
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if attribute_keys.len() != 1 {
            return;
        }
        let linked_path = strong_paths[0].clone();
        let display_name = resources.display_name_for_path(&linked_path);
        if let Some(attribute) = self.objects.get_mut(&attribute_keys[0]) {
            apply_path_link(attribute, &linked_path, display_name);
        }
    }

    fn link_attribute_to_best_path(&mut self, attribute_key: &str, timestamp: f64) {
        let strong_paths = self.strong_targetish_paths_near(timestamp);
        if strong_paths.len() > 1 {
            if let Some(attribute) = self.objects.get_mut(attribute_key) {
                mark_ambiguous_path_link(attribute, strong_paths.len(), &strong_paths);
            }
            return;
        }
        let Some(path) = strong_paths.first() else {
            return;
        };
        let attribute_keys = self
            .objects
            .iter()
            .filter(|(_, object)| object.handle_kind == ObjectHandleKind::AttributeGuid)
            .filter(|(_, object)| {
                (timestamp - object.last_seen_at).abs() <= ATTRIBUTE_PATH_LINK_WINDOW_SECONDS
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if attribute_keys.len() != 1 || attribute_keys[0] != attribute_key {
            return;
        }
        let display_name = self
            .objects
            .values()
            .find(|object| object.object_path.as_deref() == Some(path.as_str()))
            .and_then(|object| object.display_name.clone());
        if let Some(attribute) = self.objects.get_mut(attribute_key) {
            apply_path_link(attribute, path, display_name);
        }
    }

    fn strong_targetish_paths_near(&self, timestamp: f64) -> Vec<String> {
        let mut paths = self
            .objects
            .values()
            .filter(|object| object.handle_kind == ObjectHandleKind::PathOnly)
            .filter(|object| {
                (timestamp - object.last_seen_at).abs() <= ATTRIBUTE_PATH_LINK_WINDOW_SECONDS
            })
            .filter_map(|object| {
                let path = object.object_path.as_deref()?;
                strong_targetish_path(object, path).then(|| path.to_owned())
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
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

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.objects.len()
    }
}

fn apply_path_link(attribute: &mut ObjectDescriptor, path: &str, display_name: Option<String>) {
    attribute.object_path = Some(path.to_owned());
    if path.contains("/Game/") {
        attribute.class_path = Some(path.to_owned());
    }
    attribute.display_name = attribute.display_name.clone().or(display_name);
    attribute.confidence = attribute.confidence.max(0.80);
    push_unique_evidence(&mut attribute.evidence, format!("linked_path:{path}"));
}

fn mark_ambiguous_path_link(attribute: &mut ObjectDescriptor, count: usize, paths: &[String]) {
    attribute.object_path = None;
    attribute.class_path = None;
    attribute.display_name = None;
    push_unique_evidence(
        &mut attribute.evidence,
        format!("ambiguous_path_link:{count}"),
    );
    for path in paths.iter().take(3) {
        push_unique_evidence(&mut attribute.evidence, format!("ambiguous_path:{path}"));
    }
}

fn strong_targetish_path(object: &ObjectDescriptor, path: &str) -> bool {
    is_targetish_path(path) && (path.starts_with("/Game/") || object.confidence >= 0.70)
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

pub fn is_targetish_path(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["monster", "boss", "enemy", "npc", "htcharacter"]
        .iter()
        .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_index::ResourceIndex;

    fn path(value: &str) -> PathCandidate {
        PathCandidate {
            value: value.to_owned(),
            byte_offset: 1,
            bit_shift: 0,
            score: 240,
        }
    }

    #[test]
    fn hp_guid_update_maintains_timeline() {
        let mut store = ObjectStateStore::default();
        let guid = [1_u8; 16];
        store.observe_hp_guid_update(1.0, guid, 900.0, None, "hp=900".to_owned());
        let key = store.observe_hp_guid_update(1.5, guid, 800.0, None, "hp=800".to_owned());
        let object = store.objects.get(&key).unwrap();
        assert_eq!(object.hp_history.len(), 2);
        assert_eq!(object.hp_current, Some(800.0));
    }

    #[test]
    fn path_candidate_is_saved_and_deduped() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        let candidate = path("/Game/Blueprints/Character/Monster/boss_07/BP.BP_C");
        store.observe_path_candidate(1.0, &candidate, &resources);
        store.observe_path_candidate(1.1, &candidate, &resources);
        assert_eq!(store.len(), 1);
        let object = store.candidates_for_damage(1.1).pop().unwrap();
        assert_eq!(object.evidence.len(), 1);
    }

    #[test]
    fn cleanup_keeps_recent_objects() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        store.observe_path_candidate(1.0, &path("/Game/Monster/old"), &resources);
        store.observe_path_candidate(25.0, &path("/Game/Monster/recent"), &resources);
        assert_eq!(store.len(), 1);
        assert!(
            store
                .objects
                .values()
                .any(|object| object.handle.contains("recent"))
        );
    }

    #[test]
    fn attribute_guid_links_nearby_unique_target_path() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        let candidate = path("/Game/Blueprints/Character/Monster/boss_07/BP_Boss_07.BP_Boss_07_C");
        store.observe_path_candidate(1.0, &candidate, &resources);
        let key = store.observe_hp_guid_update(1.1, [2_u8; 16], 900.0, None, "hp=900".to_owned());
        let object = store.objects.get(&key).unwrap();
        assert!(
            object
                .object_path
                .as_deref()
                .is_some_and(|path| path.contains("boss_07"))
        );
        assert_eq!(object.display_name.as_deref(), Some("BP_Boss_07"));
        assert!(
            object
                .evidence
                .iter()
                .any(|evidence| evidence.starts_with("linked_path:"))
        );
    }

    #[test]
    fn attribute_guid_does_not_link_ambiguous_target_paths() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        store.observe_path_candidate(
            1.0,
            &path("/Game/Blueprints/Character/Monster/boss_07/BP_Boss_07.BP_Boss_07_C"),
            &resources,
        );
        store.observe_path_candidate(
            1.05,
            &path("/Game/Blueprints/Character/Monster/boss_08/BP_Boss_08.BP_Boss_08_C"),
            &resources,
        );
        let key = store.observe_hp_guid_update(1.1, [3_u8; 16], 900.0, None, "hp=900".to_owned());
        let object = store.objects.get(&key).unwrap();
        assert!(object.object_path.is_none());
        assert!(object.display_name.is_none());
        assert!(
            object
                .evidence
                .iter()
                .any(|evidence| evidence == "ambiguous_path_link:2")
        );
    }

    #[test]
    fn path_does_not_link_when_multiple_attribute_guids_nearby() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        let first = store.observe_hp_guid_update(1.0, [4_u8; 16], 900.0, None, "hp=900".to_owned());
        let second =
            store.observe_hp_guid_update(1.05, [5_u8; 16], 800.0, None, "hp=800".to_owned());
        store.observe_path_candidate(
            1.1,
            &path("/Game/Blueprints/Character/Monster/boss_09/BP_Boss_09.BP_Boss_09_C"),
            &resources,
        );
        assert!(store.objects.get(&first).unwrap().object_path.is_none());
        assert!(store.objects.get(&second).unwrap().object_path.is_none());
    }

    #[test]
    fn weak_targetish_path_does_not_replace_unique_strong_path_link() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        let strong_path = "/Game/Blueprints/Character/Monster/boss_07/BP_Boss_07.BP_Boss_07_C";
        let weak_path = "HTCharacterEnemy";

        store.observe_path_candidate(1.0, &path(strong_path), &resources);
        let key = store.observe_hp_guid_update(1.1, [6_u8; 16], 900.0, None, "hp=900".to_owned());
        store.observe_path_candidate(1.2, &path(weak_path), &resources);

        let object = store.objects.get(&key).unwrap();
        assert_eq!(object.object_path.as_deref(), Some(strong_path));
        assert_eq!(object.display_name.as_deref(), Some("BP_Boss_07"));
        assert!(
            object
                .evidence
                .iter()
                .any(|evidence| evidence == &format!("linked_path:{strong_path}"))
        );
        assert!(
            !object
                .evidence
                .iter()
                .any(|evidence| evidence == &format!("linked_path:{weak_path}"))
        );
    }

    #[test]
    fn path_first_does_not_link_multiple_attribute_guids_to_same_strong_path() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        let strong_path = "/Game/Blueprints/Character/Monster/boss_07/BP_Boss_07.BP_Boss_07_C";

        store.observe_path_candidate(1.0, &path(strong_path), &resources);
        let first = store.observe_hp_guid_update(1.1, [7_u8; 16], 900.0, None, "hp=900".to_owned());
        let second =
            store.observe_hp_guid_update(1.2, [8_u8; 16], 800.0, None, "hp=800".to_owned());

        assert_eq!(
            store.objects.get(&first).unwrap().object_path.as_deref(),
            Some(strong_path)
        );
        assert!(store.objects.get(&second).unwrap().object_path.is_none());
        let linked_count = store
            .objects
            .values()
            .filter(|object| object.handle_kind == ObjectHandleKind::AttributeGuid)
            .filter(|object| object.object_path.as_deref() == Some(strong_path))
            .count();
        assert_eq!(linked_count, 1);
    }
}
