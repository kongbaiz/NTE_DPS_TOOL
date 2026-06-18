use std::collections::HashSet;

use crate::model::Hit;
use crate::object_state::{
    ObjectDescriptor, ObjectHandleKind, ObjectStateStore, is_ignored_non_target_path,
    is_precise_target_path, is_targetish_path,
};
use crate::resource_index::ResourceIndex;
use crate::target_instance::TargetInstanceStore;
use crate::ue_bitstream::PathCandidate;

const HP_MATCH_TOLERANCE_ABSOLUTE: f64 = 2.0;
const HP_MATCH_TOLERANCE_RATIO: f64 = 0.002;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TargetConfidence {
    #[default]
    Unknown,
    Possible,
    Probable,
    Confirmed,
}

impl TargetConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Possible => "possible",
            Self::Probable => "probable",
            Self::Confirmed => "confirmed",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Possible => 1,
            Self::Probable => 2,
            Self::Confirmed => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TargetCandidate {
    pub handle: String,
    pub handle_kind: ObjectHandleKind,
    pub target_name: Option<String>,
    pub target_path: Option<String>,
    pub score: i32,
    pub confidence: TargetConfidence,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TargetResolutionSummary {
    pub target_id: Option<String>,
    pub target_name: Option<String>,
    pub target_context: Vec<String>,
    pub score: i32,
    pub confidence: TargetConfidence,
    pub direct_hp_evidence: bool,
}

#[derive(Default)]
pub struct TargetResolver;

impl TargetResolver {
    pub fn resolve_for_hit(
        &self,
        hit: &Hit,
        store: &ObjectStateStore,
        packet_paths: &[PathCandidate],
        resources: &ResourceIndex,
    ) -> Vec<TargetCandidate> {
        self.resolve_for_hit_with_instances(hit, store, None, packet_paths, resources)
    }

    pub fn resolve_for_hit_with_instances(
        &self,
        hit: &Hit,
        store: &ObjectStateStore,
        instances: Option<&TargetInstanceStore>,
        packet_paths: &[PathCandidate],
        resources: &ResourceIndex,
    ) -> Vec<TargetCandidate> {
        let mut candidates = store
            .candidates_for_damage(hit.timestamp)
            .into_iter()
            .map(|object| score_object(hit, object, store, resources))
            .collect::<Vec<_>>();
        if let Some(resolution) = instances.and_then(|instances| instances.resolve_hit(hit)) {
            candidates.push(TargetCandidate {
                handle: resolution.instance_id,
                handle_kind: ObjectHandleKind::RuntimeInstance,
                target_name: Some(resolution.target_name),
                target_path: Some(resolution.canonical_path),
                score: resolution.score,
                confidence: resolution.confidence,
                reasons: vec![resolution.reason],
            });
        }
        for path in packet_paths.iter().filter(|path| {
            !is_ignored_non_target_path(&path.value)
                && (resources.resolved_name_for_path(&path.value).is_some()
                    || is_targetish_path(&path.value))
        }) {
            if candidates.iter().any(|candidate| {
                candidate.target_path.as_deref() == Some(path.value.as_str())
                    || candidate.handle == path.value
            }) {
                continue;
            }
            let mut score = 25;
            let mut reasons = vec![format!("path_candidate:{}", path.value)];
            let target_name = resources.resolved_name_for_path(&path.value);
            if path.value.starts_with("/Game/") {
                score += 15;
                reasons.push("game_path_target_keyword".to_owned());
            }
            if target_name.is_some() {
                score += 35;
                reasons.push("resolved_target_name_table".to_owned());
            }
            let confidence = confidence_for_score(score);
            candidates.push(TargetCandidate {
                handle: path.value.clone(),
                handle_kind: ObjectHandleKind::PathOnly,
                target_name,
                target_path: Some(path.value.clone()),
                score,
                confidence,
                reasons,
            });
        }
        if candidates.len() > 1 {
            let strong = candidates
                .iter()
                .filter(|candidate| candidate.score >= 35)
                .count();
            let unique_strong = unique_strong_candidate_count(&candidates);
            if strong > 1 && unique_strong > 1 && !has_direct_hp_match(&candidates) {
                for candidate in &mut candidates {
                    candidate.score -= 20;
                    candidate
                        .reasons
                        .push("conflict:multiple_candidates".to_owned());
                    candidate.confidence = confidence_for_score(candidate.score);
                }
            }
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.score));
        candidates
    }

    #[allow(dead_code)]
    pub fn apply_to_hit(
        &self,
        hit: &mut Hit,
        store: &ObjectStateStore,
        packet_paths: &[PathCandidate],
        resources: &ResourceIndex,
    ) -> Vec<TargetCandidate> {
        let candidates = self.resolve_for_hit(hit, store, packet_paths, resources);
        apply_candidates_to_hit(hit, &candidates);
        candidates
    }

    pub fn apply_to_hit_with_summary(
        &self,
        hit: &mut Hit,
        store: &ObjectStateStore,
        instances: &TargetInstanceStore,
        packet_paths: &[PathCandidate],
        resources: &ResourceIndex,
    ) -> TargetResolutionSummary {
        let candidates = self.resolve_for_hit_with_instances(
            hit,
            store,
            Some(instances),
            packet_paths,
            resources,
        );
        apply_candidates_to_hit(hit, &candidates);
        TargetResolutionSummary::from_hit_and_candidates(hit, &candidates)
    }
}

impl TargetResolutionSummary {
    pub fn from_hit_and_candidates(hit: &Hit, candidates: &[TargetCandidate]) -> Self {
        let Some(top) = candidates.first() else {
            return Self {
                target_id: hit.target_id.clone(),
                target_name: hit.target_name.clone(),
                target_context: hit.target_context.clone(),
                score: 0,
                confidence: TargetConfidence::Unknown,
                direct_hp_evidence: false,
            };
        };
        Self {
            target_id: hit.target_id.clone(),
            target_name: hit.target_name.clone(),
            target_context: hit.target_context.clone(),
            score: top.score,
            confidence: top.confidence,
            direct_hp_evidence: candidate_has_direct_hp_evidence(top),
        }
    }
}

fn apply_candidates_to_hit(hit: &mut Hit, candidates: &[TargetCandidate]) {
    let Some(top) = candidates.first() else {
        hit.target_context
            .push("confidence=unknown score=0 reason=no_target_candidate".to_owned());
        return;
    };
    hit.target_context.push(format!(
        "confidence={} score={}",
        top.confidence.as_str(),
        top.score
    ));
    hit.target_context.extend(
        top.reasons
            .iter()
            .take(6)
            .map(|reason| format!("reason={reason}")),
    );
    if let Some(path) = &top.target_path
        && is_ignored_non_target_path(path)
    {
        hit.target_context
            .push(format!("ignored_non_target_path={path}"));
    }
    append_target_handle_context(hit, top);
    if top.confidence != TargetConfidence::Unknown {
        hit.target_id = Some(if top.handle_kind == ObjectHandleKind::RuntimeInstance {
            top.handle.clone()
        } else {
            format!("{}:{}", top.handle_kind.label(), top.handle)
        });
    }
    if let Some(resolution) = resolved_display_name_from_candidates(candidates) {
        if let Some(path) = &resolution.target_path {
            hit.target_context.push(format!("target_path={path}"));
        } else if let Some(path) = &top.target_path
            && !is_ignored_non_target_path(path)
        {
            hit.target_context.push(format!("target_path={path}"));
        }
        hit.target_context
            .push(format!("target_name={}", resolution.name));
        hit.target_context.push(format!(
            "target_name_resolution={}",
            if resolution.ambiguous {
                "table_resolved_ambiguous"
            } else {
                "table_resolved"
            }
        ));
        if resolution.ambiguous {
            hit.target_context.push(format!(
                "target_name_candidates={}",
                resolution.distinct_names.join(",")
            ));
        }
        hit.target_name = Some(resolution.name);
    } else if let Some(path) = &top.target_path
        && !is_ignored_non_target_path(path)
    {
        hit.target_context.push(format!("target_path={path}"));
    }
    if candidates.len() > 1 {
        hit.target_context
            .push(format!("candidate_count={}", candidates.len()));
    }
}

fn append_target_handle_context(hit: &mut Hit, candidate: &TargetCandidate) {
    match candidate.handle_kind {
        ObjectHandleKind::AttributeGuid => {
            hit.target_context.push(format!(
                "target_handle_candidate=AttributeGuid:{}",
                candidate.handle
            ));
            hit.target_context
                .push(format!("boss_hp_guid={}", candidate.handle));
        }
        ObjectHandleKind::NetRefHandleCandidate => {
            hit.target_context.push(format!(
                "target_handle_candidate=NetRefHandleCandidate:{}",
                candidate.handle
            ));
            if let Some(token) = candidate.handle.strip_prefix("currenthp:") {
                hit.target_context.push(format!("current_hp_token={token}"));
            }
        }
        _ => {}
    }
}

#[derive(Clone, Debug)]
struct DisplayNameResolution {
    name: String,
    target_path: Option<String>,
    distinct_names: Vec<String>,
    ambiguous: bool,
}

fn resolved_display_name_from_candidates(
    candidates: &[TargetCandidate],
) -> Option<DisplayNameResolution> {
    let mut named = candidates
        .iter()
        .filter(|candidate| candidate.target_name.is_some())
        .filter(|candidate| {
            !candidate
                .target_path
                .as_deref()
                .is_some_and(is_ignored_non_target_path)
        })
        .collect::<Vec<_>>();
    if named.is_empty() {
        return None;
    }
    named.sort_by(|left, right| compare_named_candidates(left, right));

    let selected = candidates
        .first()
        .filter(|candidate| {
            candidate.target_name.is_some()
                && !candidate
                    .target_path
                    .as_deref()
                    .is_some_and(is_ignored_non_target_path)
        })
        .unwrap_or(named[0]);
    let selected_name = selected.target_name.clone()?;
    let selected_target_path = selected.target_path.clone();
    let mut distinct_names = Vec::new();
    for candidate in named {
        let Some(name) = &candidate.target_name else {
            continue;
        };
        if !distinct_names.iter().any(|existing| existing == name) {
            distinct_names.push(name.clone());
        }
    }
    Some(DisplayNameResolution {
        name: selected_name,
        target_path: selected_target_path,
        ambiguous: distinct_names.len() > 1,
        distinct_names,
    })
}

fn compare_named_candidates(left: &TargetCandidate, right: &TargetCandidate) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| non_path_only_rank(right).cmp(&non_path_only_rank(left)))
        .then_with(|| target_path_specificity(right).cmp(&target_path_specificity(left)))
        .then_with(|| left.handle_kind.label().cmp(right.handle_kind.label()))
        .then_with(|| candidate_stable_identity(left).cmp(candidate_stable_identity(right)))
}

fn non_path_only_rank(candidate: &TargetCandidate) -> u8 {
    u8::from(candidate.handle_kind != ObjectHandleKind::PathOnly)
}

fn target_path_specificity(candidate: &TargetCandidate) -> usize {
    let Some(path) = candidate.target_path.as_deref() else {
        return 0;
    };
    let mut score = path.len();
    if is_precise_target_path(path) {
        score += 10_000;
    }
    if path.starts_with("/Game/") {
        score += 1_000;
    }
    score
}

fn candidate_stable_identity(candidate: &TargetCandidate) -> &str {
    candidate
        .target_path
        .as_deref()
        .unwrap_or(candidate.handle.as_str())
}

fn score_object(
    hit: &Hit,
    object: &ObjectDescriptor,
    store: &ObjectStateStore,
    resources: &ResourceIndex,
) -> TargetCandidate {
    let mut score = 0;
    let mut reasons = Vec::new();
    let path_only = object.handle_kind == ObjectHandleKind::PathOnly;
    let identity_without_hp = matches!(
        object.handle_kind,
        ObjectHandleKind::NetGuidCandidate | ObjectHandleKind::NetRefHandleCandidate
    ) && object.hp_current.is_none();
    let target_path = object
        .object_path
        .clone()
        .or_else(|| object.class_path.clone());
    if let Some(path) = &target_path {
        let resolved_name = resources.resolved_name_for_path(path);
        if is_targetish_path(path) {
            score += if path_only || identity_without_hp {
                25
            } else {
                30
            };
            reasons.push(format!("near_object_path:{path}"));
            if path.starts_with("/Game/") {
                score += if path_only || identity_without_hp {
                    15
                } else {
                    40
                };
                reasons.push("game_path_target_keyword".to_owned());
            }
            if identity_without_hp {
                reasons.push("net_identity_path_anchor_unconfirmed".to_owned());
            }
        }
        if resolved_name.is_some() {
            score += 35;
            reasons.push("resolved_target_name_table".to_owned());
        }
    }

    let target_name = target_path
        .as_deref()
        .and_then(|path| resources.resolved_name_for_path(path));

    let mut direct_hp_match = false;
    for observation_pair in object
        .hp_history
        .as_slices()
        .0
        .windows(2)
        .chain(object.hp_history.as_slices().1.windows(2))
    {
        let previous = &observation_pair[0];
        let current = &observation_pair[1];
        let delta = previous.current - current.current;
        let time_delta = (current.timestamp - hit.timestamp).abs();
        if time_delta <= 1.0 && nearly_equal(delta, hit.damage, hit.damage) {
            score += 50;
            direct_hp_match = true;
            reasons.push(format!("{}:{delta:.0}", hp_delta_reason(object)));
            let time_bonus = ((1.0 - time_delta).max(0.0) * 20.0).round() as i32;
            if time_bonus > 0 {
                score += time_bonus;
                reasons.push(format!("hp_update_time_bonus:{time_bonus}"));
            }
        }
        if time_delta <= 1.0
            && nearly_equal(previous.current, hit.target_hp_before, hit.target_hp_before)
            && nearly_equal(current.current, hit.target_hp_after, hit.target_hp_after)
        {
            score += 50;
            direct_hp_match = true;
            reasons.push(format!(
                "{}:{}->{}",
                hp_timeline_reason(object),
                previous.evidence,
                current.evidence
            ));
        }
    }

    if !direct_hp_match
        && let Some(current_hp) = object.hp_current
        && (current_hp - hit.target_hp_after).abs() <= hp_tolerance(hit.target_hp_before)
    {
        score += 20;
        reasons.push("last_hp_close_to_hit_after".to_owned());
    }
    if hit.target_max_hp > 500_000.0 && object.hp_current.is_some() {
        score += 5;
        reasons.push("target_max_hp_only_weak".to_owned());
    }

    let high_conf_targets = store
        .objects_near_time(hit.timestamp, 1.0)
        .into_iter()
        .filter(|candidate| {
            candidate.confidence >= 0.65
                && candidate
                    .object_path
                    .as_deref()
                    .or(candidate.class_path.as_deref())
                    .is_some_and(is_targetish_path)
        })
        .filter_map(|candidate| {
            candidate
                .object_path
                .as_deref()
                .or(candidate.class_path.as_deref())
                .map(str::to_owned)
        })
        .collect::<HashSet<_>>()
        .len();
    if !path_only && high_conf_targets == 1 && target_path.as_deref().is_some_and(is_targetish_path)
    {
        let bonus = if object.hp_current.is_some() { 35 } else { 25 };
        score += bonus;
        reasons.push(format!("single_high_confidence_target_window:{bonus}"));
    }

    let confidence = confidence_for_score(score);
    TargetCandidate {
        handle: object.handle.clone(),
        handle_kind: object.handle_kind.clone(),
        target_name,
        target_path,
        score,
        confidence,
        reasons,
    }
}

fn has_direct_hp_match(candidates: &[TargetCandidate]) -> bool {
    candidates.iter().any(candidate_has_direct_hp_evidence)
}

fn unique_strong_candidate_count(candidates: &[TargetCandidate]) -> usize {
    candidates
        .iter()
        .filter(|candidate| candidate.score >= 35)
        .map(candidate_conflict_key)
        .collect::<HashSet<_>>()
        .len()
}

fn candidate_conflict_key(candidate: &TargetCandidate) -> String {
    candidate
        .target_path
        .clone()
        .unwrap_or_else(|| format!("{}:{}", candidate.handle_kind.label(), candidate.handle))
}

fn candidate_has_direct_hp_evidence(candidate: &TargetCandidate) -> bool {
    candidate.reasons.iter().any(|reason| {
        reason.starts_with("hp_guid_timeline_match")
            || reason.starts_with("net_target_hp_timeline_match")
            || reason.starts_with("hp_timeline_match")
            || reason.starts_with("boss_hp_delta_match")
            || reason.starts_with("net_target_hp_delta_match")
            || reason.starts_with("hp_delta_match")
            || (candidate.handle_kind == ObjectHandleKind::AttributeGuid
                && reason == "last_hp_close_to_hit_after")
            || (candidate.handle_kind == ObjectHandleKind::NetRefHandleCandidate
                && reason == "last_hp_close_to_hit_after")
    })
}

fn hp_delta_reason(object: &ObjectDescriptor) -> &'static str {
    match object.handle_kind {
        ObjectHandleKind::AttributeGuid => "boss_hp_delta_match",
        ObjectHandleKind::NetRefHandleCandidate => "net_target_hp_delta_match",
        _ => "hp_delta_match",
    }
}

fn hp_timeline_reason(object: &ObjectDescriptor) -> &'static str {
    match object.handle_kind {
        ObjectHandleKind::AttributeGuid => "hp_guid_timeline_match",
        ObjectHandleKind::NetRefHandleCandidate => "net_target_hp_timeline_match",
        _ => "hp_timeline_match",
    }
}

fn confidence_for_score(score: i32) -> TargetConfidence {
    if score >= 90 {
        TargetConfidence::Confirmed
    } else if score >= 60 {
        TargetConfidence::Probable
    } else if score >= 35 {
        TargetConfidence::Possible
    } else {
        TargetConfidence::Unknown
    }
}

fn nearly_equal(left: f64, right: f64, scale: f64) -> bool {
    (left - right).abs() <= hp_tolerance(scale)
}

fn hp_tolerance(scale: f64) -> f64 {
    HP_MATCH_TOLERANCE_ABSOLUTE.max(scale.abs() * HP_MATCH_TOLERANCE_RATIO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Hit;
    use crate::object_state::ObjectStateStore;

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
    fn single_boss_hp_delta_generates_probable_or_confirmed_target() {
        let mut store = ObjectStateStore::default();
        let guid = [7_u8; 16];
        store.observe_hp_guid_update(9.8, guid, 1000.0, None, "hp=1000".to_owned());
        store.observe_hp_guid_update(10.1, guid, 900.0, None, "hp=900".to_owned());
        let resolver = TargetResolver;
        let candidates = resolver.resolve_for_hit(&hit(), &store, &[], &ResourceIndex::default());
        assert!(candidates[0].score >= 60);
        assert!(
            candidates[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("boss_hp_delta_match"))
        );
    }

    #[test]
    fn multiple_candidates_without_direct_evidence_are_not_confirmed() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        store.observe_path_candidate(
            10.0,
            &PathCandidate {
                value: "/Game/Blueprints/Character/Monster/boss_07/BP_Boss_07.BP_Boss_07_C"
                    .to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            },
            &resources,
        );
        store.observe_path_candidate(
            10.0,
            &PathCandidate {
                value: "/Game/Blueprints/Character/Monster/boss_08/BP_Boss_08.BP_Boss_08_C"
                    .to_owned(),
                byte_offset: 8,
                bit_shift: 0,
                score: 240,
            },
            &resources,
        );
        let candidates = TargetResolver.resolve_for_hit(&hit(), &store, &[], &resources);
        assert_ne!(candidates[0].confidence, TargetConfidence::Confirmed);
    }

    #[test]
    fn only_target_max_hp_stays_possible_or_lower() {
        let mut store = ObjectStateStore::default();
        let guid = [9_u8; 16];
        store.observe_hp_guid_update(10.0, guid, 900.0, None, "hp=900".to_owned());
        let mut hit = hit();
        hit.target_max_hp = 1_000_000.0;
        let candidates =
            TargetResolver.resolve_for_hit(&hit, &store, &[], &ResourceIndex::default());
        assert!(candidates[0].score < 60);
    }

    #[test]
    fn single_linked_hp_target_without_exact_delta_is_probable() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        store.observe_path_candidate(
            9.5,
            &PathCandidate {
                value: "WorldBoss_Boss33".to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            },
            &resources,
        );
        store.observe_hp_guid_update(10.0, [13_u8; 16], 900.0, None, "hp=900".to_owned());

        let candidates = TargetResolver.resolve_for_hit(&hit(), &store, &[], &resources);

        assert_eq!(candidates[0].confidence, TargetConfidence::Probable);
        assert!(
            candidates[0]
                .reasons
                .iter()
                .any(|reason| reason == "single_high_confidence_target_window:35")
        );
    }

    #[test]
    fn resolved_bare_boss_id_path_fills_target_name() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::load_default();
        store.observe_path_candidate(
            9.5,
            &PathCandidate {
                value: "Boss_07_BP_DiyBoss".to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 140,
            },
            &resources,
        );
        store.observe_hp_guid_update(
            10.0,
            [0xd7_u8; 16],
            1_976_104.0,
            None,
            "boss_hp=d73c=1976104".to_owned(),
        );
        let mut hit = hit();
        hit.target_hp_before = 1_978_005.0;
        hit.target_hp_after = 1_976_104.0;
        hit.target_max_hp = 1_978_005.0;

        TargetResolver.apply_to_hit(&mut hit, &store, &[], &resources);

        assert_eq!(hit.target_name.as_deref(), Some("塞润尼缇"));
        assert!(hit.target_context.iter().any(|entry| {
            entry == "target_path=Boss_07_BP_DiyBoss" || entry == "target_name=塞润尼缇"
        }));
    }

    #[test]
    fn unknown_does_not_fill_target_name() {
        let store = ObjectStateStore::default();
        let mut hit = hit();
        let candidates =
            TargetResolver.apply_to_hit(&mut hit, &store, &[], &ResourceIndex::default());
        assert!(candidates.is_empty());
        assert!(hit.target_name.is_none());
        assert!(hit.target_context[0].contains("unknown"));
    }

    #[test]
    fn reasons_include_readable_evidence() {
        let path = PathCandidate {
            value: "/Game/Blueprints/Character/Monster/boss_07/BP.BP_C".to_owned(),
            byte_offset: 1,
            bit_shift: 0,
            score: 240,
        };
        let mut hit = hit();
        let candidates = TargetResolver.apply_to_hit(
            &mut hit,
            &ObjectStateStore::default(),
            &[path],
            &ResourceIndex::default(),
        );
        assert!(
            candidates[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("/Game/"))
        );
        assert!(
            hit.target_context
                .iter()
                .any(|item| item.contains("reason="))
        );
    }

    #[test]
    fn path_only_resolved_name_fills_target_name_without_hp_evidence() {
        let path = PathCandidate {
            value: "mon_04_BP".to_owned(),
            byte_offset: 1,
            bit_shift: 0,
            score: 240,
        };
        let resources = ResourceIndex::load_default();
        let mut hit = hit();
        let candidates = TargetResolver.apply_to_hit(
            &mut hit,
            &ObjectStateStore::default(),
            &[path],
            &resources,
        );
        assert_eq!(candidates[0].handle_kind, ObjectHandleKind::PathOnly);
        assert_eq!(hit.target_name.as_deref(), Some("迷失种"));
        assert!(
            hit.target_context
                .iter()
                .any(|item| item == "target_name_resolution=table_resolved")
        );
    }

    #[test]
    fn resolved_precise_monster_tag_fills_target_name() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::load_default();
        store.observe_path_candidate(
            30.952,
            &PathCandidate {
                value: "mon_04_BP".to_owned(),
                byte_offset: 28,
                bit_shift: 2,
                score: 240,
            },
            &resources,
        );
        let mut hit = hit();
        hit.timestamp = 33.958;
        hit.target_hp_before = 2404.0;
        hit.target_hp_after = 0.0;
        hit.target_max_hp = 31950.0;

        TargetResolver.apply_to_hit(&mut hit, &store, &[], &resources);

        assert_eq!(hit.target_name.as_deref(), Some("迷失种"));
        assert!(
            hit.target_context
                .iter()
                .any(|entry| entry == "target_name=迷失种")
        );
        assert!(
            hit.target_context
                .iter()
                .any(|entry| entry.contains("probable"))
        );
    }

    #[test]
    fn monster_folder_buff_path_is_not_target_candidate() {
        let path = PathCandidate {
            value: "/Game/Blueprints/Character/Monster/mon_16/Trial/buff_Trial_LockHP100_BP"
                .to_owned(),
            byte_offset: 1,
            bit_shift: 0,
            score: 240,
        };

        let candidates = TargetResolver.resolve_for_hit(
            &hit(),
            &ObjectStateStore::default(),
            &[path],
            &ResourceIndex::default(),
        );

        assert!(candidates.is_empty());
    }

    #[test]
    fn direct_hp_match_avoids_conflict_penalty() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        let guid = [10_u8; 16];
        store.observe_hp_guid_update(9.8, guid, 1000.0, None, "hp=1000".to_owned());
        store.observe_hp_guid_update(10.1, guid, 900.0, None, "hp=900".to_owned());
        store.observe_path_candidate(
            10.0,
            &PathCandidate {
                value: "/Game/Monster/OtherBoss".to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            },
            &resources,
        );

        let candidates = TargetResolver.resolve_for_hit(&hit(), &store, &[], &resources);
        let direct = candidates
            .iter()
            .find(|candidate| candidate.handle_kind == ObjectHandleKind::AttributeGuid)
            .expect("attribute candidate should exist");
        assert!(
            direct
                .reasons
                .iter()
                .any(|reason| reason.starts_with("hp_guid_timeline_match"))
        );
        assert!(
            !direct
                .reasons
                .iter()
                .any(|reason| reason == "conflict:multiple_candidates")
        );
    }

    #[test]
    fn boss_hp_delta_counts_as_direct_hp_evidence() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        let guid = [11_u8; 16];
        store.observe_hp_guid_update(9.8, guid, 1000.0, None, "hp=1000".to_owned());
        store.observe_hp_guid_update(10.1, guid, 900.0, None, "hp=900".to_owned());
        let mut hit = hit();
        hit.target_hp_before = 0.0;
        hit.target_hp_after = 0.0;
        store.observe_path_candidate(
            10.0,
            &PathCandidate {
                value: "/Game/Monster/PathOnlyBoss".to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            },
            &resources,
        );

        let candidates = TargetResolver.resolve_for_hit(&hit, &store, &[], &resources);
        let direct = candidates
            .iter()
            .find(|candidate| candidate.handle_kind == ObjectHandleKind::AttributeGuid)
            .expect("attribute candidate should exist");
        assert!(
            direct
                .reasons
                .iter()
                .any(|reason| reason.starts_with("boss_hp_delta_match"))
        );
        assert!(
            !direct
                .reasons
                .iter()
                .any(|reason| reason == "conflict:multiple_candidates")
        );
    }

    #[test]
    fn unknown_attribute_candidate_still_writes_backfill_handle_context() {
        let mut store = ObjectStateStore::default();
        let guid = [
            0xc5, 0x5c, 0x88, 0x59, 0x03, 0xe8, 0x49, 0x40, 0x93, 0x6c, 0x7d, 0x09, 0x15, 0xdc,
            0x8c, 0xad,
        ];
        store.observe_hp_guid_update(10.0, guid, 12_345.0, None, "hp=12345".to_owned());
        let mut hit = hit();
        hit.target_hp_before = 900_000.0;
        hit.target_hp_after = 880_000.0;
        hit.target_max_hp = 1_304_923.0;

        TargetResolver.apply_to_hit(&mut hit, &store, &[], &ResourceIndex::default());

        assert!(hit.target_id.is_none());
        assert!(hit.target_context.iter().any(|entry| {
            entry == "target_handle_candidate=AttributeGuid:c55c885903e84940936c7d0915dc8cad"
        }));
        assert!(
            hit.target_context
                .iter()
                .any(|entry| entry == "boss_hp_guid=c55c885903e84940936c7d0915dc8cad")
        );
    }

    #[test]
    fn net_target_hp_timeline_can_resolve_probable_target() {
        let mut store = ObjectStateStore::default();
        let token = [12_u8; 40];
        store.observe_net_target_hp_update(
            9.8,
            "currenthp",
            &token,
            1000.0,
            None,
            "hp=1000".to_owned(),
        );
        store.observe_net_target_hp_update(
            10.1,
            "currenthp",
            &token,
            900.0,
            None,
            "hp=900".to_owned(),
        );

        let candidates =
            TargetResolver.resolve_for_hit(&hit(), &store, &[], &ResourceIndex::default());

        assert_eq!(
            candidates[0].handle_kind,
            ObjectHandleKind::NetRefHandleCandidate
        );
        assert!(candidates[0].score >= 60);
        assert!(candidates[0].reasons.iter().any(|reason| {
            reason.starts_with("net_target_hp_delta_match")
                || reason.starts_with("net_target_hp_timeline_match")
        }));
    }

    #[test]
    fn multiple_path_only_candidates_do_not_hide_resolved_names() {
        let resources = ResourceIndex::load_default();
        let paths = [
            PathCandidate {
                value: "mon_04_BP".to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            },
            PathCandidate {
                value: "Boss_07_BP_DiyBoss".to_owned(),
                byte_offset: 8,
                bit_shift: 0,
                score: 240,
            },
        ];
        let mut hit = hit();
        let candidates =
            TargetResolver.apply_to_hit(&mut hit, &ObjectStateStore::default(), &paths, &resources);
        assert!(candidates.iter().all(|candidate| {
            candidate.handle_kind == ObjectHandleKind::PathOnly
                && candidate.confidence.rank() <= TargetConfidence::Possible.rank()
        }));
        assert!(hit.target_name.is_some());
        assert!(hit.target_context.iter().any(|entry| {
            entry.starts_with("target_name_candidates=")
                && entry.contains("迷失种")
                && entry.contains("塞润尼缇")
        }));
    }

    #[test]
    fn unnamed_high_score_hp_candidate_does_not_mask_named_path_candidate() {
        let mut store = ObjectStateStore::default();
        let guid = [0x44_u8; 16];
        store.observe_hp_guid_update(9.8, guid, 1000.0, None, "hp=1000".to_owned());
        store.observe_hp_guid_update(10.1, guid, 900.0, None, "hp=900".to_owned());
        let resources = ResourceIndex::load_default();
        let path = PathCandidate {
            value: "mon_04_BP".to_owned(),
            byte_offset: 0,
            bit_shift: 0,
            score: 240,
        };
        let mut hit = hit();

        TargetResolver.apply_to_hit(&mut hit, &store, &[path], &resources);

        assert!(
            hit.target_id
                .as_deref()
                .is_some_and(|id| id.starts_with("AttributeGuid:"))
        );
        assert_eq!(hit.target_name.as_deref(), Some("迷失种"));
    }

    #[test]
    fn unresolved_path_candidate_does_not_fill_target_name() {
        let path = PathCandidate {
            value: "/Game/Monster/UnknownBoss".to_owned(),
            byte_offset: 1,
            bit_shift: 0,
            score: 240,
        };
        let mut hit = hit();

        TargetResolver.apply_to_hit(
            &mut hit,
            &ObjectStateStore::default(),
            &[path],
            &ResourceIndex::default(),
        );

        assert!(hit.target_name.is_none());
    }

    #[test]
    fn ignored_buff_top_candidate_does_not_write_target_path() {
        let mut hit = hit();
        let candidates = vec![
            TargetCandidate {
                handle: "buff".to_owned(),
                handle_kind: ObjectHandleKind::PathOnly,
                target_name: None,
                target_path: Some("Default__Buff_Boss07_Night_Weaktime_C".to_owned()),
                score: 100,
                confidence: TargetConfidence::Confirmed,
                reasons: vec!["near_object_path:Default__Buff_Boss07_Night_Weaktime_C".to_owned()],
            },
            TargetCandidate {
                handle: "boss".to_owned(),
                handle_kind: ObjectHandleKind::PathOnly,
                target_name: Some("塞润尼缇".to_owned()),
                target_path: Some("Boss_07_BP_WorldBoss".to_owned()),
                score: 80,
                confidence: TargetConfidence::Probable,
                reasons: vec!["resolved_target_name_table".to_owned()],
            },
        ];

        apply_candidates_to_hit(&mut hit, &candidates);

        assert_eq!(hit.target_name.as_deref(), Some("塞润尼缇"));
        assert!(
            hit.target_context
                .iter()
                .any(|entry| entry == "target_path=Boss_07_BP_WorldBoss")
        );
        assert!(
            hit.target_context
                .iter()
                .any(|entry| entry
                    == "ignored_non_target_path=Default__Buff_Boss07_Night_Weaktime_C")
        );
        assert!(
            !hit.target_context
                .iter()
                .any(|entry| entry == "target_path=Default__Buff_Boss07_Night_Weaktime_C")
        );
    }

    #[test]
    fn duplicate_net_identity_path_anchor_does_not_create_target_conflict() {
        let mut store = ObjectStateStore::default();
        let resources = ResourceIndex::default();
        let path = "/Game/Blueprints/Character/Monster/boss_07/BP_Boss_07.BP_Boss_07_C";
        store.observe_path_candidate(
            10.0,
            &PathCandidate {
                value: path.to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            },
            &resources,
        );
        store.observe_path_handle_candidate(
            10.0,
            ObjectHandleKind::NetGuidCandidate,
            "0x12345678".to_owned(),
            path,
            &resources,
            format!("net_identity:netguid32=0x12345678 path_anchor:{path}@0:0"),
            82,
        );

        let candidates = TargetResolver.resolve_for_hit(&hit(), &store, &[], &resources);

        assert!(candidates.len() >= 2);
        assert!(candidates.iter().all(|candidate| {
            !candidate
                .reasons
                .iter()
                .any(|reason| reason == "conflict:multiple_candidates")
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate
                .reasons
                .iter()
                .any(|reason| reason == "net_identity_path_anchor_unconfirmed")
        }));
    }
}
