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
const RECENT_DEATH_STALE_TARGET_WINDOW_SECONDS: f64 = 8.0;

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
            candidates.push(TargetCandidate {
                handle: path.value.clone(),
                handle_kind: ObjectHandleKind::PathOnly,
                target_name: None,
                target_path: None,
                score: 5,
                confidence: TargetConfidence::Unknown,
                reasons: vec![
                    format!("path_candidate:{}", path.value),
                    "packet_path_target_name_suppressed".to_owned(),
                ],
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
        if let Some(instances) = instances {
            let active_named = instances.active_named_instance_count(hit.timestamp);
            if active_named > 1 {
                for candidate in &mut candidates {
                    candidate
                        .reasons
                        .push(format!("active_named_instances:{active_named}"));
                }
            }
        }
        candidates.sort_by(compare_target_candidates);
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
    append_unresolved_context(hit, candidates);
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
    } else if candidate_can_display_target_name(top, candidates)
        && let Some(path) = &top.target_path
        && !is_ignored_non_target_path(path)
    {
        hit.target_context.push(format!("target_path={path}"));
    }
    if candidates.len() > 1 {
        hit.target_context
            .push(format!("candidate_count={}", candidates.len()));
    }
    if top.confidence != TargetConfidence::Unknown {
        hit.target_id = Some(target_identity_for_candidate(top, hit));
    }
}

fn target_identity_for_candidate(candidate: &TargetCandidate, hit: &Hit) -> String {
    let base = if candidate.handle_kind == ObjectHandleKind::RuntimeInstance {
        candidate.handle.clone()
    } else {
        format!("{}:{}", candidate.handle_kind.label(), candidate.handle)
    };
    if !target_identity_needs_resolved_target(&candidate.handle_kind) {
        return base;
    }
    if let Some(path) = target_context_value(&hit.target_context, "target_path") {
        return format!("{base}|path={path}");
    }
    if let Some(name) = hit
        .target_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return format!("{base}|target={name}");
    }
    base
}

fn target_identity_needs_resolved_target(handle_kind: &ObjectHandleKind) -> bool {
    matches!(
        handle_kind,
        ObjectHandleKind::AttributeGuid
            | ObjectHandleKind::NetRefHandleCandidate
            | ObjectHandleKind::NetGuidCandidate
    )
}

fn target_context_value<'a>(context: &'a [String], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    context
        .iter()
        .find_map(|value| value.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "None")
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
    if candidates
        .first()
        .is_some_and(candidate_recent_death_suppressed)
    {
        return None;
    }
    let mut named = candidates
        .iter()
        .filter(|candidate| candidate_can_display_target_name(candidate, candidates))
        .collect::<Vec<_>>();
    if named.is_empty() {
        return None;
    }
    named.sort_by(|left, right| compare_named_candidates(left, right));

    let selected = candidates
        .first()
        .filter(|candidate| candidate_can_display_target_name(candidate, candidates))
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

fn candidate_recent_death_suppressed(candidate: &TargetCandidate) -> bool {
    candidate
        .reasons
        .iter()
        .any(|reason| reason == "recent_death_suppressed_stale_target")
}

fn candidate_can_display_target_name(
    candidate: &TargetCandidate,
    candidates: &[TargetCandidate],
) -> bool {
    if candidate.target_name.is_none()
        || candidate_recent_death_suppressed(candidate)
        || candidate
            .target_path
            .as_deref()
            .is_some_and(is_ignored_non_target_path)
        || candidate.reasons.iter().any(|reason| {
            matches!(
                reason.as_str(),
                "path_only_target_name_suppressed"
                    | "hp_handle_path_without_direct_hp_suppressed"
                    | "net_identity_path_anchor_unconfirmed"
                    | "runtime_unique_active_named_instance"
            )
        })
    {
        return false;
    }
    if candidate.handle_kind == ObjectHandleKind::RuntimeInstance {
        return candidate
            .reasons
            .iter()
            .any(|reason| reason.starts_with("runtime_alias:"))
            || (candidate
                .reasons
                .iter()
                .any(|reason| reason == "runtime_hp_timeline_unique")
                && !has_named_direct_hp_conflict(candidate, candidates));
    }
    candidate_has_direct_hp_evidence(candidate)
        && !has_named_direct_hp_conflict(candidate, candidates)
}

fn has_named_direct_hp_conflict(
    candidate: &TargetCandidate,
    candidates: &[TargetCandidate],
) -> bool {
    let candidate_key = candidate_conflict_key(candidate);
    candidates
        .iter()
        .filter(|other| candidate_has_direct_hp_evidence(other))
        .filter(|other| other.target_name.is_some())
        .map(candidate_conflict_key)
        .any(|key| key != candidate_key)
}

fn append_unresolved_context(hit: &mut Hit, candidates: &[TargetCandidate]) {
    if candidates.iter().any(|candidate| {
        candidate.target_path.is_some()
            && candidate.target_name.is_none()
            && !candidate_has_direct_hp_evidence(candidate)
    }) {
        hit.target_context
            .push("target_unresolved=resource_name_missing".to_owned());
        if let Some(path) = candidates.iter().find_map(|candidate| {
            candidate
                .target_path
                .as_deref()
                .filter(|path| !is_ignored_non_target_path(path))
        }) {
            hit.target_context
                .push(format!("unresolved_target_path={path}"));
        }
    }
    if candidates.iter().any(|candidate| {
        candidate_has_direct_hp_evidence(candidate) && candidate.target_name.is_none()
    }) {
        hit.target_context
            .push("target_unresolved=hp_evidence_without_table_name".to_owned());
    }
    let named_direct_hp = candidates
        .iter()
        .filter(|candidate| candidate_has_direct_hp_evidence(candidate))
        .filter(|candidate| candidate.target_name.is_some())
        .map(candidate_conflict_key)
        .collect::<HashSet<_>>();
    if named_direct_hp.len() > 1
        || candidates.iter().any(|candidate| {
            candidate
                .reasons
                .iter()
                .any(|reason| reason == "conflict:multiple_candidates")
        })
    {
        hit.target_context
            .push("target_unresolved=ambiguous_multi_target".to_owned());
        hit.target_context
            .push("target_suppressed=ambiguous_multi_target".to_owned());
    }
}

fn compare_named_candidates(left: &TargetCandidate, right: &TargetCandidate) -> std::cmp::Ordering {
    candidate_sort_score(right)
        .cmp(&candidate_sort_score(left))
        .then_with(|| right.score.cmp(&left.score))
        .then_with(|| non_path_only_rank(right).cmp(&non_path_only_rank(left)))
        .then_with(|| target_path_specificity(right).cmp(&target_path_specificity(left)))
        .then_with(|| left.handle_kind.label().cmp(right.handle_kind.label()))
        .then_with(|| candidate_stable_identity(left).cmp(candidate_stable_identity(right)))
}

fn compare_target_candidates(
    left: &TargetCandidate,
    right: &TargetCandidate,
) -> std::cmp::Ordering {
    candidate_sort_score(right)
        .cmp(&candidate_sort_score(left))
        .then_with(|| right.score.cmp(&left.score))
        .then_with(|| right.confidence.rank().cmp(&left.confidence.rank()))
        .then_with(|| non_path_only_rank(right).cmp(&non_path_only_rank(left)))
        .then_with(|| target_path_specificity(right).cmp(&target_path_specificity(left)))
        .then_with(|| left.handle_kind.label().cmp(right.handle_kind.label()))
        .then_with(|| candidate_stable_identity(left).cmp(candidate_stable_identity(right)))
}

fn candidate_sort_score(candidate: &TargetCandidate) -> i32 {
    let mut score = candidate.score;
    if candidate_has_direct_hp_evidence(candidate) {
        score += 60;
    } else if candidate_has_weak_hp_proximity(candidate) {
        score += 5;
    }
    score += match candidate.handle_kind {
        ObjectHandleKind::RuntimeInstance => 50,
        ObjectHandleKind::AttributeGuid => 35,
        ObjectHandleKind::NetRefHandleCandidate | ObjectHandleKind::NetGuidCandidate => {
            if candidate_unconfirmed_path_anchor(candidate) {
                -50
            } else {
                20
            }
        }
        ObjectHandleKind::PathOnly => 0,
        ObjectHandleKind::Unknown => -20,
    };
    if candidate.target_name.is_none() {
        score -= 10;
    }
    score
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
    let raw_target_path = object
        .object_path
        .clone()
        .or_else(|| object.class_path.clone());
    let suppress_stale_target = suppress_stale_target_after_recent_death(
        hit,
        object,
        raw_target_path.as_deref(),
        path_only || identity_without_hp,
        store,
    );
    if suppress_stale_target {
        reasons.push("recent_death_suppressed_stale_target".to_owned());
    }
    let mut target_path = if suppress_stale_target {
        None
    } else {
        raw_target_path
    };
    if let Some(path) = &target_path {
        let resolved_name = resources.resolved_name_for_path(path);
        if is_targetish_path(path) {
            score += if path_only || identity_without_hp {
                15
            } else {
                30
            };
            reasons.push(format!("near_object_path:{path}"));
            if path.starts_with("/Game/") {
                score += if path_only || identity_without_hp {
                    5
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
            score += if identity_without_hp { 15 } else { 35 };
            reasons.push("resolved_target_name_table".to_owned());
        }
    }

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
        reasons.push("target_max_hp_only_weak".to_owned());
    }

    if path_only {
        if target_path.is_some() {
            reasons.push("path_only_target_name_suppressed".to_owned());
        }
        target_path = None;
        score = score.min(10);
    }

    if ((object.handle_kind == ObjectHandleKind::NetRefHandleCandidate
        && object.handle.starts_with("currenthp:"))
        || object.handle_kind == ObjectHandleKind::AttributeGuid)
        && !direct_hp_match
    {
        if target_path.is_some() {
            reasons.push("hp_handle_path_without_direct_hp_suppressed".to_owned());
        }
        target_path = None;
        score = score.min(20);
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

    let target_name = target_path
        .as_deref()
        .and_then(|path| resources.resolved_name_for_path(path));

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
    })
}

fn candidate_has_weak_hp_proximity(candidate: &TargetCandidate) -> bool {
    candidate
        .reasons
        .iter()
        .any(|reason| reason == "last_hp_close_to_hit_after")
}

fn suppress_stale_target_after_recent_death(
    hit: &Hit,
    object: &ObjectDescriptor,
    target_path: Option<&str>,
    path_only: bool,
    store: &ObjectStateStore,
) -> bool {
    if hit.target_hp_after <= 1.0 {
        return false;
    }
    if object.dead_at.is_some_and(|dead_at| {
        (hit.timestamp - dead_at).abs() <= RECENT_DEATH_STALE_TARGET_WINDOW_SECONDS
    }) || object.hp_history.iter().any(|observation| {
        observation.current <= 1.0
            && (hit.timestamp - observation.timestamp).abs()
                <= RECENT_DEATH_STALE_TARGET_WINDOW_SECONDS
    }) {
        return true;
    }
    path_only
        && target_path.is_some_and(|path| {
            store.path_recently_died(
                path,
                hit.timestamp,
                RECENT_DEATH_STALE_TARGET_WINDOW_SECONDS,
            )
        })
}

fn candidate_unconfirmed_path_anchor(candidate: &TargetCandidate) -> bool {
    candidate
        .reasons
        .iter()
        .any(|reason| reason == "net_identity_path_anchor_unconfirmed")
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
    use crate::object_state::ObjectHandleKind;
    use crate::target_instance::{TargetAlias, TargetAliasKind, TargetInstanceStore};

    fn test_hit(timestamp: f64, before: f64, after: f64, damage: f64) -> Hit {
        Hit {
            timestamp,
            char_id: 1,
            char_name: "tester".to_owned(),
            char_known: true,
            damage,
            byte_offset: 10,
            bit_shift: 0,
            char_source: "test".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: before,
            target_hp_after: after,
            target_max_hp: before,
            target_hp_percent: after / before * 100.0,
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

    fn resources_with_targets() -> ResourceIndex {
        let mut resources = ResourceIndex::default();
        resources.insert_test_target("/Game/Monster/Boss_001_BP.Boss_001_BP_C", "Target A");
        resources.insert_test_target("/Game/Monster/Boss_002_BP.Boss_002_BP_C", "Target B");
        resources
    }

    fn observe_direct_hp(
        store: &mut ObjectStateStore,
        resources: &ResourceIndex,
        guid: [u8; 16],
        path: &str,
        before: f64,
        after: f64,
    ) {
        let handle = hex::encode(guid);
        store.observe_path_handle_candidate(
            1.0,
            ObjectHandleKind::AttributeGuid,
            handle,
            path,
            resources,
            "test_path_anchor".to_owned(),
            255,
        );
        store.observe_hp_guid_update(1.0, guid, before, Some(before), "before".to_owned());
        store.observe_hp_guid_update(1.1, guid, after, Some(before), "after".to_owned());
    }

    #[test]
    fn direct_hp_unique_candidate_displays_only_that_target() {
        let resources = resources_with_targets();
        let mut store = ObjectStateStore::default();
        observe_direct_hp(
            &mut store,
            &resources,
            [1; 16],
            "/Game/Monster/Boss_001_BP.Boss_001_BP_C",
            1000.0,
            900.0,
        );
        let mut hit = test_hit(1.1, 1000.0, 900.0, 100.0);
        let resolver = TargetResolver;
        let paths = [PathCandidate {
            value: "/Game/Monster/Boss_002_BP.Boss_002_BP_C".to_owned(),
            byte_offset: 20,
            bit_shift: 0,
            score: 255,
        }];

        resolver.apply_to_hit_with_summary(
            &mut hit,
            &store,
            &TargetInstanceStore::default(),
            &paths,
            &resources,
        );

        assert_eq!(hit.target_name.as_deref(), Some("Target A"));
        assert!(
            hit.target_context
                .iter()
                .any(|entry| { entry == "target_path=/Game/Monster/Boss_001_BP.Boss_001_BP_C" })
        );
    }

    #[test]
    fn ambiguous_direct_hp_candidates_do_not_display_name() {
        let resources = resources_with_targets();
        let mut store = ObjectStateStore::default();
        observe_direct_hp(
            &mut store,
            &resources,
            [1; 16],
            "/Game/Monster/Boss_001_BP.Boss_001_BP_C",
            1000.0,
            900.0,
        );
        observe_direct_hp(
            &mut store,
            &resources,
            [2; 16],
            "/Game/Monster/Boss_002_BP.Boss_002_BP_C",
            1000.0,
            900.0,
        );
        let mut hit = test_hit(1.1, 1000.0, 900.0, 100.0);

        TargetResolver.apply_to_hit_with_summary(
            &mut hit,
            &store,
            &TargetInstanceStore::default(),
            &[],
            &resources,
        );

        assert_eq!(hit.target_name, None);
        assert!(
            hit.target_context
                .iter()
                .any(|entry| { entry == "target_unresolved=ambiguous_multi_target" })
        );
    }

    #[test]
    fn multiple_active_instances_require_hit_alias_or_direct_hp() {
        let resources = resources_with_targets();
        let store = ObjectStateStore::default();
        let mut instances = TargetInstanceStore::default();
        instances.observe_runtime_mapping(
            1.0,
            "/Game/Monster/Boss_001_BP.Boss_001_BP_C".to_owned(),
            "Target A".to_owned(),
            vec![TargetAlias::new(TargetAliasKind::NetGuid32, "aaaa")],
        );
        instances.observe_runtime_mapping(
            1.0,
            "/Game/Monster/Boss_002_BP.Boss_002_BP_C".to_owned(),
            "Target B".to_owned(),
            vec![TargetAlias::new(TargetAliasKind::NetGuid32, "bbbb")],
        );
        let mut hit = test_hit(1.2, 1000.0, 900.0, 100.0);

        TargetResolver.apply_to_hit_with_summary(&mut hit, &store, &instances, &[], &resources);

        assert_eq!(hit.target_name, None);
    }

    #[test]
    fn missing_resource_name_suppresses_display_even_with_hp_evidence() {
        let resources = ResourceIndex::default();
        let mut store = ObjectStateStore::default();
        observe_direct_hp(
            &mut store,
            &resources,
            [3; 16],
            "/Game/Monster/Boss_003_BP.Boss_003_BP_C",
            1000.0,
            900.0,
        );
        let mut hit = test_hit(1.1, 1000.0, 900.0, 100.0);

        TargetResolver.apply_to_hit_with_summary(
            &mut hit,
            &store,
            &TargetInstanceStore::default(),
            &[],
            &resources,
        );

        assert_eq!(hit.target_name, None);
        assert!(
            hit.target_context
                .iter()
                .any(|entry| { entry == "target_unresolved=hp_evidence_without_table_name" })
        );
    }

    #[test]
    fn weak_last_hp_close_does_not_display_target_name() {
        let resources = resources_with_targets();
        let mut store = ObjectStateStore::default();
        let guid = [4; 16];
        let handle = hex::encode(guid);
        store.observe_path_handle_candidate(
            1.0,
            ObjectHandleKind::AttributeGuid,
            handle,
            "/Game/Monster/Boss_001_BP.Boss_001_BP_C",
            &resources,
            "test_path_anchor".to_owned(),
            255,
        );
        store.observe_hp_guid_update(1.0, guid, 900.0, Some(1000.0), "current".to_owned());
        let mut hit = test_hit(1.0, 1000.0, 900.0, 100.0);

        TargetResolver.apply_to_hit_with_summary(
            &mut hit,
            &store,
            &TargetInstanceStore::default(),
            &[],
            &resources,
        );

        assert_eq!(hit.target_name, None);
        assert!(
            hit.target_context
                .iter()
                .any(|entry| entry == "reason=last_hp_close_to_hit_after")
        );
    }
}
