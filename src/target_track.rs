#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};

use crate::model::Hit;
use crate::object_state::is_ignored_non_target_path;
use crate::target_fact::DamageHitFact;
use crate::target_identity::{
    canonical_target_key_for_path, canonical_target_key_from_name_and_path,
};
use crate::target_resolver::{TargetConfidence, TargetResolutionSummary};

const ACTIVE_TRACK_WINDOW_SECONDS: f64 = 3.5;
const HP_CONTINUITY_WINDOW_SECONDS: f64 = 3.5;
const HANDLE_QUARANTINE_SECONDS: f64 = 8.0;
const HP_MATCH_TOLERANCE_ABSOLUTE: f64 = 2.0;
const HP_MATCH_TOLERANCE_RATIO: f64 = 0.002;
const MAX_HP_TIMELINE_PER_TRACK: usize = 16;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TargetTrackId(pub String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetLifecycle {
    Provisional,
    Active,
    Dying,
    Dead,
    Quarantined,
    Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetLabelState {
    Unlabeled,
    Provisional,
    Locked,
    Conflicted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackIdentitySource {
    RuntimeMapping,
    RuntimeAlias,
    NonHpAlias,
    DirectHpTimeline,
    DirectHpDelta,
}

#[derive(Clone, Debug)]
pub struct HpTimelinePoint {
    pub timestamp: f64,
    pub hp_before: f64,
    pub hp_after: f64,
    pub reported_max_hp_observation: Option<f64>,
    pub hit_uid: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TargetTrack {
    pub track_id: TargetTrackId,
    pub generation: u32,
    pub canonical_target_key: String,
    pub target_name: Option<String>,
    pub display_target_path: Option<String>,
    pub observed_paths: HashSet<String>,
    pub label_state: TargetLabelState,
    pub lifecycle: TargetLifecycle,
    pub first_seen_at: f64,
    pub last_seen_at: f64,
    pub last_damage_at: Option<f64>,
    pub hp_timeline: VecDeque<HpTimelinePoint>,
    pub non_hp_aliases: HashSet<String>,
    pub hp_handles: HashSet<String>,
    pub assigned_hit_uids: HashSet<String>,
    pub conflict_flags: HashSet<String>,
    pub source: Option<TrackIdentitySource>,
    quarantined_at: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct TrackPacketContext {
    pub canonical_target_paths: HashSet<String>,
    pub canonical_target_names: HashSet<String>,
    pub hp_handle_keys: HashSet<String>,
    pub has_multiple_canonical_targets: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttributionResult {
    pub target_track_id: Option<TargetTrackId>,
    pub generation: Option<u32>,
    pub target_name: Option<String>,
    pub target_path: Option<String>,
    pub projected: bool,
    pub ambiguous: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct TargetTrackStore {
    tracks: HashMap<String, TargetTrack>,
    non_hp_alias_index: HashMap<String, String>,
    hp_handle_index: HashMap<String, String>,
    generation_by_canonical_key: HashMap<String, u32>,
}

impl TargetTrackStore {
    pub fn attribute_damage_hit(
        &mut self,
        hit: &mut Hit,
        summary: &mut TargetResolutionSummary,
        context: TrackPacketContext,
    ) -> AttributionResult {
        if hit.direction == "incoming" {
            return AttributionResult::default();
        }

        let fact = DamageHitFact::from(&*hit);
        let mut result = AttributionResult::default();
        if can_learn_named_hit(hit, summary) {
            result = self.observe_named_hit(hit, summary, &fact);
        }
        if hit.target_name.is_none() || !has_target_track_id(hit) {
            let track_result = self.attribute_unnamed_hit(hit, summary, &fact, &context);
            if track_result.target_track_id.is_some() || track_result.ambiguous {
                result = track_result;
            }
        }

        if hit.target_hp_after <= 1.0 {
            if result.target_track_id.is_none() {
                result = self.attribute_terminal_hit(hit, summary, &fact);
            }
            if let Some(track_id) = result.target_track_id.as_ref() {
                self.mark_track_dead(track_id, hit.timestamp, &fact.hit_uid);
            }
        }
        result
    }

    pub fn active_track_count(&self, timestamp: f64) -> usize {
        self.active_tracks(timestamp).count()
    }

    pub fn track(&self, track_id: &TargetTrackId) -> Option<&TargetTrack> {
        self.tracks.get(&track_id.0)
    }

    fn observe_named_hit(
        &mut self,
        hit: &mut Hit,
        summary: &mut TargetResolutionSummary,
        fact: &DamageHitFact,
    ) -> AttributionResult {
        let Some(source) = identity_source_from_hit(hit, summary) else {
            return AttributionResult::default();
        };
        let Some(name) = hit
            .target_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
        else {
            return AttributionResult::default();
        };
        let Some(path) = target_context_value(&hit.target_context, "target_path")
            .filter(|path| !is_ignored_non_target_path(path))
            .map(str::to_owned)
        else {
            return AttributionResult::default();
        };
        let canonical_key = canonical_target_key_from_name_and_path(&name, &path)
            .unwrap_or_else(|| fallback_canonical_key(&path));
        let non_hp_aliases = non_hp_alias_keys(hit.target_id.as_ref(), &hit.target_context);
        let hp_handles = hp_handle_keys(hit.target_id.as_ref(), &hit.target_context);

        if self.alias_conflict(hit, &non_hp_aliases, &canonical_key, &name, false)
            || self.alias_conflict(hit, &hp_handles, &canonical_key, &name, true)
        {
            summary.target_context = hit.target_context.clone();
            return AttributionResult {
                ambiguous: true,
                reason: Some("alias_conflict".to_owned()),
                ..Default::default()
            };
        }

        let track_key = self
            .track_for_non_hp_aliases(&non_hp_aliases)
            .or_else(|| self.single_active_same_canonical_key_track(&canonical_key, fact.timestamp))
            .or_else(|| self.track_for_hp_handle_same_canonical_key(&hp_handles, &canonical_key))
            .unwrap_or_else(|| self.create_track(&canonical_key, &path, fact.timestamp));

        let Some(track) = self.tracks.get_mut(&track_key) else {
            return AttributionResult::default();
        };
        if track.canonical_target_key != canonical_key {
            push_unique_context(
                &mut hit.target_context,
                "target_conflict=locked_path_mismatch".to_owned(),
            );
            track
                .conflict_flags
                .insert("locked_path_mismatch".to_owned());
            summary.target_context = hit.target_context.clone();
            return AttributionResult {
                ambiguous: true,
                reason: Some("locked_path_mismatch".to_owned()),
                ..Default::default()
            };
        }
        if track
            .target_name
            .as_deref()
            .is_some_and(|existing| existing != name)
        {
            push_unique_context(
                &mut hit.target_context,
                "target_conflict=locked_name_mismatch".to_owned(),
            );
            track
                .conflict_flags
                .insert("locked_name_mismatch".to_owned());
            summary.target_context = hit.target_context.clone();
            return AttributionResult {
                ambiguous: true,
                reason: Some("locked_name_mismatch".to_owned()),
                ..Default::default()
            };
        }

        track.target_name = Some(name.clone());
        track.display_target_path = Some(path.clone());
        track.observed_paths.insert(path.clone());
        track.label_state = TargetLabelState::Locked;
        track.lifecycle = if hit.target_hp_after <= 1.0 {
            TargetLifecycle::Dying
        } else {
            TargetLifecycle::Active
        };
        track.source = Some(stronger_source(track.source, source));
        update_track_damage(track, fact);
        for key in non_hp_aliases {
            track.non_hp_aliases.insert(key.clone());
            self.non_hp_alias_index.insert(key, track_key.clone());
        }
        for key in hp_handles {
            track.hp_handles.insert(key.clone());
            self.hp_handle_index.insert(key, track_key.clone());
        }
        project_track_to_hit(track, hit, summary, false, "track_named_hit");
        AttributionResult {
            target_track_id: Some(track.track_id.clone()),
            generation: Some(track.generation),
            target_name: track.target_name.clone(),
            target_path: track.display_target_path.clone(),
            projected: false,
            ambiguous: false,
            reason: Some("track_named_hit".to_owned()),
        }
    }

    fn attribute_unnamed_hit(
        &mut self,
        hit: &mut Hit,
        summary: &mut TargetResolutionSummary,
        fact: &DamageHitFact,
        context: &TrackPacketContext,
    ) -> AttributionResult {
        if !can_attribute_unknown(hit) {
            push_track_reject(
                hit,
                self.active_track_count(fact.timestamp),
                0,
                "hard_conflict",
            );
            summary.target_context = hit.target_context.clone();
            return AttributionResult::default();
        }

        let non_hp_aliases = non_hp_alias_keys(hit.target_id.as_ref(), &hit.target_context);
        if let Some(track_key) =
            self.unique_active_track_for_aliases(&non_hp_aliases, fact.timestamp, false)
        {
            return self.project_track_key_to_hit(
                &track_key,
                hit,
                summary,
                fact,
                "unique_non_hp_alias",
            );
        }

        let hp_handles = hp_handle_keys(hit.target_id.as_ref(), &hit.target_context);
        if let Some(track_key) =
            self.unique_active_track_for_aliases(&hp_handles, fact.timestamp, true)
        {
            return self.project_track_key_to_hit(
                &track_key,
                hit,
                summary,
                fact,
                "unique_locked_hp_handle",
            );
        }

        let matches = self.matching_track_keys_for_hit(fact);
        append_track_diagnostics(hit, self.active_track_count(fact.timestamp), matches.len());
        if matches.len() == 1 {
            let reason = if is_dot_or_followup_damage(hit) {
                "single_active_track_dot"
            } else {
                "single_active_track_projection"
            };
            return self.project_track_key_to_hit(&matches[0], hit, summary, fact, reason);
        }
        if matches.len() > 1 {
            push_ambiguous_context(hit);
            replace_or_push_context(&mut hit.target_context, "track_decision", "not_projected");
            replace_or_push_context(
                &mut hit.target_context,
                "track_reject_reason",
                "multiple_matching_tracks",
            );
            summary.target_context = hit.target_context.clone();
            return AttributionResult {
                ambiguous: true,
                reason: Some("multiple_matching_tracks".to_owned()),
                ..Default::default()
            };
        }
        if matches.is_empty()
            && self.active_track_count(fact.timestamp) == 0
            && is_dot_or_followup_damage(hit)
            && !context.has_multiple_canonical_targets
            && context.canonical_target_paths.len() == 1
            && context.canonical_target_names.len() == 1
        {
            let path = context
                .canonical_target_paths
                .iter()
                .next()
                .expect("checked one path")
                .clone();
            let name = context
                .canonical_target_names
                .iter()
                .next()
                .expect("checked one name")
                .clone();
            let canonical_key = canonical_target_key_from_name_and_path(&name, &path)
                .unwrap_or_else(|| fallback_canonical_key(&path));
            let track_key = self.create_track(&canonical_key, &path, fact.timestamp);
            if let Some(track) = self.tracks.get_mut(&track_key) {
                track.target_name = Some(name);
                track.display_target_path = Some(path.clone());
                track.observed_paths.insert(path);
                track.label_state = TargetLabelState::Locked;
                track.lifecycle = TargetLifecycle::Active;
                track.source = Some(TrackIdentitySource::DirectHpTimeline);
            }
            return self.project_track_key_to_hit(
                &track_key,
                hit,
                summary,
                fact,
                "single_active_track_dot",
            );
        }
        push_track_reject(
            hit,
            self.active_track_count(fact.timestamp),
            matches.len(),
            if context.has_multiple_canonical_targets {
                "no_matching_track_multiple_canonical_targets"
            } else {
                "hp_stream_mismatch"
            },
        );
        summary.target_context = hit.target_context.clone();
        AttributionResult::default()
    }

    fn attribute_terminal_hit(
        &mut self,
        hit: &mut Hit,
        summary: &mut TargetResolutionSummary,
        fact: &DamageHitFact,
    ) -> AttributionResult {
        let mut matches = self
            .active_tracks(fact.timestamp)
            .filter(|track| terminal_hit_matches_track(track, fact))
            .map(|track| track.track_id.0.clone())
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        if matches.len() == 1 {
            self.project_track_key_to_hit(
                &matches[0],
                hit,
                summary,
                fact,
                "terminal_hit_active_track",
            )
        } else {
            if matches.len() > 1 {
                push_ambiguous_context(hit);
                summary.target_context = hit.target_context.clone();
            }
            AttributionResult::default()
        }
    }

    fn project_track_key_to_hit(
        &mut self,
        track_key: &str,
        hit: &mut Hit,
        summary: &mut TargetResolutionSummary,
        fact: &DamageHitFact,
        reason: &str,
    ) -> AttributionResult {
        let Some(track) = self.tracks.get_mut(track_key) else {
            return AttributionResult::default();
        };
        if track.label_state != TargetLabelState::Locked || track.target_name.is_none() {
            update_track_damage(track, fact);
            return AttributionResult {
                target_track_id: Some(track.track_id.clone()),
                generation: Some(track.generation),
                reason: Some(reason.to_owned()),
                ..Default::default()
            };
        }
        update_track_damage(track, fact);
        project_track_to_hit(track, hit, summary, true, reason);
        AttributionResult {
            target_track_id: Some(track.track_id.clone()),
            generation: Some(track.generation),
            target_name: track.target_name.clone(),
            target_path: track.display_target_path.clone(),
            projected: true,
            ambiguous: false,
            reason: Some(reason.to_owned()),
        }
    }

    fn mark_track_dead(&mut self, track_id: &TargetTrackId, timestamp: f64, hit_uid: &str) {
        let Some(track) = self.tracks.get_mut(&track_id.0) else {
            return;
        };
        track.lifecycle = TargetLifecycle::Dead;
        track.last_seen_at = timestamp;
        track.last_damage_at = Some(timestamp);
        track.assigned_hit_uids.insert(hit_uid.to_owned());
        track.quarantined_at = Some(timestamp);
    }

    fn create_track(&mut self, canonical_key: &str, display_path: &str, timestamp: f64) -> String {
        let generation = self
            .generation_by_canonical_key
            .entry(canonical_key.to_owned())
            .and_modify(|value| *value += 1)
            .or_insert(1);
        let track_key = format!("{canonical_key}#{generation}");
        self.tracks.insert(
            track_key.clone(),
            TargetTrack {
                track_id: TargetTrackId(track_key.clone()),
                generation: *generation,
                canonical_target_key: canonical_key.to_owned(),
                target_name: None,
                display_target_path: Some(display_path.to_owned()),
                observed_paths: HashSet::from([display_path.to_owned()]),
                label_state: TargetLabelState::Provisional,
                lifecycle: TargetLifecycle::Provisional,
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                last_damage_at: None,
                hp_timeline: VecDeque::new(),
                non_hp_aliases: HashSet::new(),
                hp_handles: HashSet::new(),
                assigned_hit_uids: HashSet::new(),
                conflict_flags: HashSet::new(),
                source: None,
                quarantined_at: None,
            },
        );
        track_key
    }

    fn track_for_non_hp_aliases(&self, aliases: &HashSet<String>) -> Option<String> {
        aliases
            .iter()
            .find_map(|key| self.non_hp_alias_index.get(key).cloned())
    }

    fn single_active_same_canonical_key_track(
        &self,
        canonical_key: &str,
        timestamp: f64,
    ) -> Option<String> {
        let mut matches = self
            .active_tracks(timestamp)
            .filter(|track| track.canonical_target_key == canonical_key)
            .map(|track| track.track_id.0.clone())
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        (matches.len() == 1).then(|| matches.remove(0))
    }

    fn track_for_hp_handle_same_canonical_key(
        &self,
        hp_handles: &HashSet<String>,
        canonical_key: &str,
    ) -> Option<String> {
        let mut matches = hp_handles
            .iter()
            .filter_map(|key| self.hp_handle_index.get(key))
            .filter_map(|track_key| self.tracks.get(track_key))
            .filter(|track| track.canonical_target_key == canonical_key)
            .map(|track| track.track_id.0.clone())
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        (matches.len() == 1).then(|| matches.remove(0))
    }

    fn unique_active_track_for_aliases(
        &self,
        aliases: &HashSet<String>,
        timestamp: f64,
        hp_handle: bool,
    ) -> Option<String> {
        if aliases.is_empty() {
            return None;
        }
        let index = if hp_handle {
            &self.hp_handle_index
        } else {
            &self.non_hp_alias_index
        };
        let mut matches = aliases
            .iter()
            .filter_map(|key| index.get(key))
            .filter_map(|track_key| self.tracks.get(track_key))
            .filter(|track| track_usable_for_exact_alias(track, timestamp, hp_handle))
            .filter(|track| !hp_handle || !hp_handle_quarantined(track, timestamp))
            .map(|track| track.track_id.0.clone())
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        (matches.len() == 1).then(|| matches.remove(0))
    }

    fn active_tracks(&self, timestamp: f64) -> impl Iterator<Item = &TargetTrack> {
        self.tracks
            .values()
            .filter(move |track| track_is_active(track, timestamp))
    }

    fn matching_track_keys_for_hit(&self, fact: &DamageHitFact) -> Vec<String> {
        let mut matches = self
            .active_tracks(fact.timestamp)
            .filter(|track| track_can_explain_hit(track, fact))
            .map(|track| track.track_id.0.clone())
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        matches
    }

    fn same_name_multi_instance_active(&self, timestamp: f64) -> bool {
        let mut names = HashMap::<&str, HashSet<u32>>::new();
        for track in self.active_tracks(timestamp) {
            let Some(name) = track.target_name.as_deref() else {
                continue;
            };
            names.entry(name).or_default().insert(track.generation);
        }
        names.values().any(|generations| generations.len() > 1)
    }

    fn alias_conflict(
        &self,
        hit: &mut Hit,
        aliases: &HashSet<String>,
        canonical_key: &str,
        name: &str,
        hp_handle: bool,
    ) -> bool {
        let index = if hp_handle {
            &self.hp_handle_index
        } else {
            &self.non_hp_alias_index
        };
        for key in aliases {
            let Some(track_key) = index.get(key) else {
                continue;
            };
            let Some(track) = self.tracks.get(track_key) else {
                continue;
            };
            if track.canonical_target_key == canonical_key
                && track.target_name.as_deref() == Some(name)
            {
                continue;
            }
            if hp_handle && hp_handle_quarantined(track, hit.timestamp) {
                push_unique_context(
                    &mut hit.target_context,
                    "target_conflict=hp_handle_reused_without_lifecycle_reset".to_owned(),
                );
            } else if track.canonical_target_key != canonical_key {
                push_unique_context(
                    &mut hit.target_context,
                    "target_conflict=locked_path_mismatch".to_owned(),
                );
            } else {
                push_unique_context(
                    &mut hit.target_context,
                    "target_conflict=locked_name_mismatch".to_owned(),
                );
            }
            return true;
        }
        false
    }
}

fn can_learn_named_hit(hit: &Hit, summary: &TargetResolutionSummary) -> bool {
    hit.target_name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
        && !hit.target_context.iter().any(|entry| {
            entry.starts_with("target_conflict=")
                || entry == "reason=recent_death_suppressed_stale_target"
                || entry == "target_suppressed=ambiguous_multi_target"
        })
        && identity_source_from_hit(hit, summary).is_some()
}

fn can_attribute_unknown(hit: &Hit) -> bool {
    hit.target_name.is_none()
        && !hit.target_context.iter().any(|entry| {
            entry.starts_with("target_conflict=")
                || entry == "reason=recent_death_suppressed_stale_target"
                || entry == "target_lifecycle=dead_or_expired"
        })
}

fn identity_source_from_hit(
    hit: &Hit,
    summary: &TargetResolutionSummary,
) -> Option<TrackIdentitySource> {
    if hit
        .target_context
        .iter()
        .any(|entry| entry == "target_name_resolution=runtime_mapping")
    {
        return Some(TrackIdentitySource::RuntimeMapping);
    }
    if hit
        .target_context
        .iter()
        .any(|entry| entry.starts_with("reason=runtime_alias:"))
    {
        return Some(TrackIdentitySource::RuntimeAlias);
    }
    if has_non_hp_alias(hit) {
        return Some(TrackIdentitySource::NonHpAlias);
    }
    if hit.target_context.iter().any(|entry| {
        entry.starts_with("reason=hp_guid_timeline_match")
            || entry.starts_with("reason=net_target_hp_timeline_match")
            || entry.starts_with("reason=hp_timeline_match")
            || entry == "reason=runtime_hp_timeline_unique"
    }) {
        return Some(TrackIdentitySource::DirectHpTimeline);
    }
    if hit.target_context.iter().any(|entry| {
        entry.starts_with("reason=boss_hp_delta_match")
            || entry.starts_with("reason=net_target_hp_delta_match")
            || entry.starts_with("reason=hp_delta_match")
    }) {
        return Some(TrackIdentitySource::DirectHpDelta);
    }
    (summary.direct_hp_evidence
        && target_context_value(&hit.target_context, "target_name_resolution")
            == Some("table_resolved"))
    .then_some(TrackIdentitySource::DirectHpTimeline)
}

fn project_track_to_hit(
    track: &TargetTrack,
    hit: &mut Hit,
    summary: &mut TargetResolutionSummary,
    projected: bool,
    reason: &str,
) {
    hit.target_id = Some(track.track_id.0.clone());
    remove_same_canonical_path_mismatch(hit, track);
    if let Some(name) = &track.target_name {
        hit.target_name = Some(name.clone());
        replace_or_push_context(&mut hit.target_context, "target_name", name);
    }
    if let Some(path) = &track.display_target_path {
        replace_or_push_context(&mut hit.target_context, "target_path", path);
    }
    replace_or_push_context(
        &mut hit.target_context,
        "canonical_target_key",
        &track.canonical_target_key,
    );
    replace_or_push_context(
        &mut hit.target_context,
        "target_track_id",
        &track.track_id.0,
    );
    replace_or_push_context(
        &mut hit.target_context,
        "target_generation",
        &track.generation.to_string(),
    );
    if projected {
        remove_stale_unresolved_context(&mut hit.target_context);
        replace_or_push_context(
            &mut hit.target_context,
            "target_name_resolution",
            if reason == "single_active_track_dot" {
                "track_dot_projection"
            } else {
                "track_continuity_projected"
            },
        );
        replace_or_push_context(&mut hit.target_context, "track_decision", "projected");
        replace_or_push_context(&mut hit.target_context, "track_reason", reason);
    }
    for path in &track.observed_paths {
        push_unique_context(
            &mut hit.target_context,
            format!("observed_target_path={path}"),
        );
    }
    if let Some(previous_max) = previous_reported_max_observation(track, hit)
        && reported_max_changed(previous_max, hit.target_max_hp)
    {
        push_unique_context(
            &mut hit.target_context,
            format!(
                "target_hp_max_changed={:.0}->{:.0}",
                previous_max, hit.target_max_hp
            ),
        );
        push_unique_context(
            &mut hit.target_context,
            "target_hp_max_unstable=true".to_owned(),
        );
    }
    push_unique_context(&mut hit.target_context, format!("reason={reason}"));
    summary.target_id = hit.target_id.clone();
    summary.target_name = hit.target_name.clone();
    summary.target_context = hit.target_context.clone();
    summary.score = summary.score.max(if projected { 85 } else { 100 });
    if summary.confidence.rank() < TargetConfidence::Probable.rank() {
        summary.confidence = TargetConfidence::Probable;
    }
}

fn update_track_damage(track: &mut TargetTrack, fact: &DamageHitFact) {
    track.last_seen_at = fact.timestamp;
    track.last_damage_at = Some(fact.timestamp);
    track.assigned_hit_uids.insert(fact.hit_uid.clone());
    track.hp_timeline.push_back(HpTimelinePoint {
        timestamp: fact.timestamp,
        hp_before: fact.hp_before,
        hp_after: fact.hp_after,
        reported_max_hp_observation: Some(fact.hp_reported_max),
        hit_uid: Some(fact.hit_uid.clone()),
    });
    while track.hp_timeline.len() > MAX_HP_TIMELINE_PER_TRACK {
        track.hp_timeline.pop_front();
    }
}

fn track_can_explain_hit(track: &TargetTrack, fact: &DamageHitFact) -> bool {
    hp_stream_matches_single_track(track, fact)
}

fn hp_stream_matches_single_track(track: &TargetTrack, fact: &DamageHitFact) -> bool {
    if !track_is_active(track, fact.timestamp) || track.hp_timeline.is_empty() {
        return false;
    }
    if fact.timestamp - track.last_seen_at > HP_CONTINUITY_WINDOW_SECONDS {
        return false;
    }
    let recent = track
        .hp_timeline
        .iter()
        .filter(|point| {
            fact.timestamp >= point.timestamp
                && fact.timestamp - point.timestamp <= HP_CONTINUITY_WINDOW_SECONDS
        })
        .collect::<Vec<_>>();
    if recent.is_empty() {
        return false;
    }
    let max_seen_hp = recent
        .iter()
        .flat_map(|point| [point.hp_before, point.hp_after])
        .fold(f64::MIN, f64::max);
    let tolerance = hp_tolerance(max_seen_hp.max(fact.hp_before));
    let last_timestamp = recent
        .iter()
        .map(|point| point.timestamp)
        .fold(f64::MIN, f64::max);
    fact.timestamp - last_timestamp <= HP_CONTINUITY_WINDOW_SECONDS
        && fact.hp_after >= 0.0
        && fact.hp_before >= fact.hp_after
        && fact.hp_after <= max_seen_hp + tolerance
}

fn terminal_hit_matches_track(track: &TargetTrack, fact: &DamageHitFact) -> bool {
    if fact.hp_after > 1.0 || !track_is_active(track, fact.timestamp) {
        return false;
    }
    let Some(last) = track.hp_timeline.back() else {
        return false;
    };
    let age = fact.timestamp - last.timestamp;
    if !(0.0..=HP_CONTINUITY_WINDOW_SECONDS).contains(&age) {
        return false;
    }
    fact.hp_before <= last.hp_before + hp_tolerance(last.hp_before.max(fact.hp_before))
}

fn track_is_active(track: &TargetTrack, timestamp: f64) -> bool {
    matches!(
        track.lifecycle,
        TargetLifecycle::Provisional | TargetLifecycle::Active | TargetLifecycle::Dying
    ) && timestamp >= track.last_seen_at
        && timestamp - track.last_seen_at <= ACTIVE_TRACK_WINDOW_SECONDS
}

fn track_usable_for_exact_alias(track: &TargetTrack, timestamp: f64, hp_handle: bool) -> bool {
    if timestamp < track.last_seen_at {
        return false;
    }
    match track.lifecycle {
        TargetLifecycle::Dead | TargetLifecycle::Expired => false,
        TargetLifecycle::Quarantined if hp_handle => false,
        TargetLifecycle::Quarantined => false,
        TargetLifecycle::Provisional | TargetLifecycle::Active | TargetLifecycle::Dying => true,
    }
}

fn hp_handle_quarantined(track: &TargetTrack, timestamp: f64) -> bool {
    track
        .quarantined_at
        .is_some_and(|at| timestamp >= at && timestamp - at <= HANDLE_QUARANTINE_SECONDS)
}

fn stronger_source(
    current: Option<TrackIdentitySource>,
    next: TrackIdentitySource,
) -> TrackIdentitySource {
    current
        .filter(|source| identity_source_rank(*source) >= identity_source_rank(next))
        .unwrap_or(next)
}

fn identity_source_rank(source: TrackIdentitySource) -> u8 {
    match source {
        TrackIdentitySource::RuntimeMapping => 5,
        TrackIdentitySource::RuntimeAlias => 4,
        TrackIdentitySource::NonHpAlias => 3,
        TrackIdentitySource::DirectHpTimeline => 2,
        TrackIdentitySource::DirectHpDelta => 1,
    }
}

fn has_non_hp_alias(hit: &Hit) -> bool {
    !non_hp_alias_keys(hit.target_id.as_ref(), &hit.target_context).is_empty()
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

fn non_hp_alias_keys(target_id: Option<&String>, context: &[String]) -> HashSet<String> {
    let mut keys = HashSet::new();
    if let Some(target_id) = target_id.filter(|id| !is_hp_alias_key(id)) {
        extend_alias_keys(&mut keys, target_id);
    }
    for key in [
        "actor_channel",
        "iris_ref32",
        "netguid32",
        "netguid_packed",
        "sdk_net_target",
    ] {
        for value in target_context_values(context, key) {
            extend_alias_keys(&mut keys, &format!("{key}:{value}"));
        }
    }
    for value in target_context_values(context, "target_handle_candidate") {
        if !is_hp_alias_key(value) {
            extend_alias_keys(&mut keys, value);
        }
    }
    keys.retain(|key| !is_hp_alias_key(key));
    keys
}

fn hp_handle_keys(target_id: Option<&String>, context: &[String]) -> HashSet<String> {
    let mut keys = HashSet::new();
    if let Some(target_id) = target_id.filter(|id| is_hp_alias_key(id)) {
        extend_alias_keys(&mut keys, target_id);
    }
    for value in target_context_values(context, "target_handle_candidate") {
        if is_hp_alias_key(value) {
            extend_alias_keys(&mut keys, value);
        }
    }
    for value in target_context_values(context, "boss_hp_guid") {
        extend_alias_keys(&mut keys, &format!("boss_hp_guid:{value}"));
    }
    for value in target_context_values(context, "current_hp_token") {
        extend_alias_keys(&mut keys, &format!("current_hp_token:{value}"));
    }
    keys.retain(|key| is_hp_alias_key(key));
    keys
}

fn extend_alias_keys(keys: &mut HashSet<String>, key: &str) {
    for key in equivalent_alias_keys(key) {
        keys.insert(key);
    }
}

fn equivalent_alias_keys(key: &str) -> Vec<String> {
    let key = normalize_alias_key(key);
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

fn is_hp_alias_key(key: &str) -> bool {
    key.starts_with("AttributeGuid:")
        || key.starts_with("boss_hp_guid:")
        || key.starts_with("current_hp_token:")
        || key.starts_with("NetRefHandleCandidate:currenthp:")
}

fn normalize_alias_key(key: &str) -> String {
    let key = key.trim().split('|').next().unwrap_or(key.trim());
    let Some((kind, value)) = key.split_once(':') else {
        return key.to_owned();
    };
    format!("{kind}:{}", value.trim().to_ascii_lowercase())
}

fn replace_or_push_context(context: &mut Vec<String>, key: &str, value: &str) {
    let prefix = format!("{key}=");
    context.retain(|entry| !entry.starts_with(&prefix));
    push_unique_context(context, format!("{key}={value}"));
}

fn push_unique_context(context: &mut Vec<String>, value: String) -> bool {
    if context.iter().any(|entry| entry == &value) {
        return false;
    }
    context.push(value);
    true
}

fn push_ambiguous_context(hit: &mut Hit) {
    push_unique_context(
        &mut hit.target_context,
        "target_unresolved=ambiguous_multi_target".to_owned(),
    );
    push_unique_context(
        &mut hit.target_context,
        "target_suppressed=ambiguous_multi_target".to_owned(),
    );
}

fn hp_tolerance(scale: f64) -> f64 {
    HP_MATCH_TOLERANCE_ABSOLUTE.max(scale.abs() * HP_MATCH_TOLERANCE_RATIO)
}

fn fallback_canonical_key(path: &str) -> String {
    format!("path:{}", path.trim().to_ascii_lowercase())
}

fn previous_reported_max_observation(track: &TargetTrack, hit: &Hit) -> Option<f64> {
    track
        .hp_timeline
        .iter()
        .rev()
        .filter(|point| {
            !(point.timestamp.to_bits() == hit.timestamp.to_bits()
                && point.hp_before.to_bits() == hit.target_hp_before.to_bits()
                && point.hp_after.to_bits() == hit.target_hp_after.to_bits())
        })
        .find_map(|point| point.reported_max_hp_observation)
}

fn reported_max_changed(previous: f64, current: f64) -> bool {
    if previous <= 0.0 || current <= 0.0 {
        return false;
    }
    (previous - current).abs() > hp_tolerance(previous.max(current))
}

fn is_dot_or_followup_damage(hit: &Hit) -> bool {
    let text = [
        hit.damage_name.as_deref(),
        hit.attack_type.as_deref(),
        hit.gameplay_effect_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();
    ["流血", "持续", "dot", "bleed", "follow"]
        .iter()
        .any(|needle| text.contains(needle))
}

fn has_target_track_id(hit: &Hit) -> bool {
    target_context_value(&hit.target_context, "target_track_id").is_some()
}

fn remove_stale_unresolved_context(context: &mut Vec<String>) {
    context.retain(|entry| {
        !matches!(
            entry.as_str(),
            "target_unresolved=ambiguous_multi_target"
                | "target_suppressed=ambiguous_multi_target"
                | "target_unresolved=hp_evidence_without_table_name"
                | "target_unresolved=resource_name_missing"
        )
    });
}

fn remove_same_canonical_path_mismatch(hit: &mut Hit, track: &TargetTrack) {
    let same_canonical = target_context_value(&hit.target_context, "target_path")
        .and_then(canonical_target_key_for_path)
        .is_some_and(|key| key == track.canonical_target_key);
    if same_canonical {
        hit.target_context
            .retain(|entry| entry != "target_conflict=locked_path_mismatch");
    }
}

fn append_track_diagnostics(context_hit: &mut Hit, active_count: usize, matching_count: usize) {
    replace_or_push_context(
        &mut context_hit.target_context,
        "track_active_count",
        &active_count.to_string(),
    );
    replace_or_push_context(
        &mut context_hit.target_context,
        "track_matching_count",
        &matching_count.to_string(),
    );
}

fn push_track_reject(hit: &mut Hit, active_count: usize, matching_count: usize, reason: &str) {
    append_track_diagnostics(hit, active_count, matching_count);
    replace_or_push_context(&mut hit.target_context, "track_decision", "not_projected");
    replace_or_push_context(&mut hit.target_context, "track_reject_reason", reason);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(timestamp: f64, before: f64, after: f64, max: f64) -> Hit {
        Hit {
            timestamp,
            char_id: 1,
            char_name: "tester".to_owned(),
            char_known: true,
            damage: (before - after).abs().max(1.0),
            byte_offset: timestamp as usize,
            bit_shift: 0,
            char_source: "test".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: before,
            target_hp_after: after,
            target_max_hp: max,
            target_hp_percent: after / max.max(1.0) * 100.0,
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

    fn named_hit(
        timestamp: f64,
        before: f64,
        after: f64,
        name: &str,
        path: &str,
        hp_handle: &str,
    ) -> (Hit, TargetResolutionSummary) {
        let mut hit = hit(timestamp, before, after, before.max(after).max(1.0));
        hit.target_name = Some(name.to_owned());
        hit.target_context = vec![
            format!("target_path={path}"),
            format!("target_name={name}"),
            "target_name_resolution=table_resolved".to_owned(),
            "reason=hp_guid_timeline_match:test".to_owned(),
            format!("boss_hp_guid={hp_handle}"),
        ];
        let summary = TargetResolutionSummary {
            target_id: hit.target_id.clone(),
            target_name: hit.target_name.clone(),
            target_context: hit.target_context.clone(),
            score: 120,
            confidence: TargetConfidence::Confirmed,
            direct_hp_evidence: true,
        };
        (hit, summary)
    }

    #[test]
    fn death_terminal_hit_keeps_target_name_before_track_closes() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            100.0,
            "斑蝶",
            "/Game/Monster/Boss_Butterfly.Boss_Butterfly_C",
            "aaa",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );

        let mut death = hit(1.5, 100.0, 0.0, 1000.0);
        let mut death_summary = TargetResolutionSummary::default();
        let result = store.attribute_damage_hit(
            &mut death,
            &mut death_summary,
            TrackPacketContext::default(),
        );

        assert_eq!(death.target_name.as_deref(), Some("斑蝶"));
        assert!(result.target_track_id.is_some());
        assert_eq!(
            store
                .track(result.target_track_id.as_ref().expect("track"))
                .expect("stored track")
                .lifecycle,
            TargetLifecycle::Dead
        );
    }

    #[test]
    fn single_boss_hp_stream_inherits_after_first_safe_identification() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1_374_729.0,
            1_368_969.0,
            "无首铁驭",
            "/Game/Monster/Boss_Headless.Boss_Headless_C",
            "aaa",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );

        for (timestamp, before, after, max) in [
            (1.2, 1_368_969.0, 1_368_056.0, 1_685_262.0),
            (1.8, 1_346_337.0, 1_341_619.0, 1_676_678.0),
            (2.4, 1_331_566.0, 1_323_571.0, 1_667_752.0),
        ] {
            let mut unknown = hit(timestamp, before, after, max);
            let mut summary = TargetResolutionSummary::default();
            let result = store.attribute_damage_hit(
                &mut unknown,
                &mut summary,
                TrackPacketContext::default(),
            );
            assert!(result.projected);
            assert_eq!(unknown.target_name.as_deref(), Some("无首铁驭"));
            assert!(
                unknown
                    .target_context
                    .iter()
                    .any(|entry| { entry == "target_name_resolution=track_continuity_projected" })
            );
        }
    }

    #[test]
    fn multi_target_ambiguous_hit_remains_unknown() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            900.0,
            "A",
            "/Game/Monster/Boss_A.Boss_A_C",
            "a",
        );
        let (mut second, mut second_summary) = named_hit(
            1.1,
            1000.0,
            920.0,
            "B",
            "/Game/Monster/Boss_B.Boss_B_C",
            "b",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        store.attribute_damage_hit(
            &mut second,
            &mut second_summary,
            TrackPacketContext::default(),
        );

        let mut unknown = hit(1.2, 900.0, 850.0, 1000.0);
        let mut summary = TargetResolutionSummary::default();
        let result =
            store.attribute_damage_hit(&mut unknown, &mut summary, TrackPacketContext::default());
        assert!(result.ambiguous);
        assert_eq!(unknown.target_name, None);
        assert!(
            unknown
                .target_context
                .iter()
                .any(|entry| entry == "target_unresolved=ambiguous_multi_target")
        );
    }

    #[test]
    fn hp_handle_reuse_after_death_does_not_rename_old_or_new_track() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            0.0,
            "A",
            "/Game/Monster/Boss_A.Boss_A_C",
            "reuse",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );

        let (mut second, mut second_summary) = named_hit(
            2.0,
            2000.0,
            1900.0,
            "B",
            "/Game/Monster/Boss_B.Boss_B_C",
            "reuse",
        );
        let result = store.attribute_damage_hit(
            &mut second,
            &mut second_summary,
            TrackPacketContext::default(),
        );

        assert!(result.ambiguous);
        assert_eq!(second.target_name.as_deref(), Some("B"));
        assert!(
            second.target_context.iter().any(|entry| {
                entry == "target_conflict=hp_handle_reused_without_lifecycle_reset"
            })
        );
    }

    #[test]
    fn same_name_multi_instance_uses_different_generation() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            0.0,
            "同名怪",
            "/Game/Monster/Mon_Same.Mon_Same_C",
            "a",
        );
        let first_result = store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        let (mut second, mut second_summary) = named_hit(
            10.0,
            1000.0,
            900.0,
            "同名怪",
            "/Game/Monster/Mon_Same.Mon_Same_C",
            "b",
        );
        let second_result = store.attribute_damage_hit(
            &mut second,
            &mut second_summary,
            TrackPacketContext::default(),
        );

        assert_ne!(first_result.generation, second_result.generation);
        assert_ne!(first_result.target_track_id, second_result.target_track_id);
    }

    #[test]
    fn weak_last_hp_close_cannot_build_confirmed_track() {
        let mut store = TargetTrackStore::default();
        let mut weak = hit(1.0, 1000.0, 900.0, 1000.0);
        weak.target_name = Some("A".to_owned());
        weak.target_context = vec![
            "target_path=/Game/Monster/Boss_A.Boss_A_C".to_owned(),
            "target_name=A".to_owned(),
            "reason=last_hp_close_to_hit_after".to_owned(),
        ];
        let mut summary = TargetResolutionSummary {
            target_name: weak.target_name.clone(),
            target_context: weak.target_context.clone(),
            confidence: TargetConfidence::Probable,
            score: 80,
            direct_hp_evidence: false,
            ..Default::default()
        };
        let result =
            store.attribute_damage_hit(&mut weak, &mut summary, TrackPacketContext::default());
        assert_eq!(result.target_track_id, None);
        assert_eq!(store.active_track_count(1.0), 0);
    }

    #[test]
    fn target_max_hp_change_does_not_break_single_track_continuity() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            900.0,
            "A",
            "/Game/Monster/Boss_A.Boss_A_C",
            "a",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        let mut unknown = hit(1.5, 850.0, 800.0, 1200.0);
        let mut summary = TargetResolutionSummary::default();
        let result =
            store.attribute_damage_hit(&mut unknown, &mut summary, TrackPacketContext::default());
        assert!(result.projected);
        assert_eq!(unknown.target_name.as_deref(), Some("A"));
    }

    #[test]
    fn max_hp_change_does_not_break_track_continuity() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1_460_091.0,
            1_458_526.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "a",
        );
        first.target_max_hp = 1_460_091.0;
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        let first_track = target_context_value(&first.target_context, "target_track_id")
            .expect("first track")
            .to_owned();
        for (timestamp, before, after, max) in [
            (1.2, 1_459_670.0, 1_438_552.0, 1_459_249.0),
            (1.5, 1_430_648.0, 1_427_852.0, 1_455_875.0),
        ] {
            let mut hit = hit(timestamp, before, after, max);
            let mut summary = TargetResolutionSummary::default();
            let result =
                store.attribute_damage_hit(&mut hit, &mut summary, TrackPacketContext::default());
            assert!(result.projected);
            assert_eq!(hit.target_name.as_deref(), Some("无首铁驭"));
            assert_eq!(
                target_context_value(&hit.target_context, "target_track_id"),
                Some(first_track.as_str())
            );
            assert_eq!(
                target_context_value(&hit.target_context, "target_generation"),
                Some("1")
            );
        }
    }

    #[test]
    fn max_hp_large_change_records_context_but_keeps_track() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1_460_091.0,
            1_458_526.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "a",
        );
        first.target_max_hp = 1_460_091.0;
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );

        let mut second = hit(1.2, 1_458_526.0, 1_355_219.0, 1_355_219.0);
        second.target_context.push("boss_hp_guid=a".to_owned());
        let mut second_summary = TargetResolutionSummary::default();
        let result = store.attribute_damage_hit(
            &mut second,
            &mut second_summary,
            TrackPacketContext::default(),
        );
        assert!(result.projected);
        assert_eq!(second.target_name.as_deref(), Some("无首铁驭"));
        assert_eq!(second.target_id.as_deref(), Some("monster:boss_13#1"));
        assert!(
            second
                .target_context
                .iter()
                .any(|entry| entry.starts_with("target_hp_max_changed="))
        );
        assert!(
            second
                .target_context
                .iter()
                .any(|entry| entry == "target_hp_max_unstable=true")
        );
        assert!(
            !second
                .target_context
                .iter()
                .any(|entry| entry.starts_with("target_unresolved="))
        );
        assert_eq!(
            target_context_value(&second.target_context, "target_generation"),
            Some("1")
        );
    }

    #[test]
    fn max_hp_same_but_different_alias_does_not_merge() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) =
            named_hit(1.0, 1000.0, 900.0, "A", "WorldBoss_Boss13", "a");
        first.target_context.push("netguid32=aaa".to_owned());
        first.target_max_hp = 1000.0;
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        let (mut second, mut second_summary) =
            named_hit(1.1, 1000.0, 900.0, "B", "WorldBoss_Boss08", "b");
        second.target_context.push("netguid32=bbb".to_owned());
        second.target_max_hp = 1000.0;
        store.attribute_damage_hit(
            &mut second,
            &mut second_summary,
            TrackPacketContext::default(),
        );

        assert_eq!(first.target_id.as_deref(), Some("monster:boss_13#1"));
        assert_eq!(second.target_id.as_deref(), Some("monster:boss_08#1"));
    }

    #[test]
    fn target_max_hp_only_weak_does_not_veto_direct_hp_named_hit() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1_460_091.0,
            1_458_526.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "a",
        );
        first
            .target_context
            .push("reason=target_max_hp_only_weak".to_owned());
        first
            .target_context
            .push("reason=hp_guid_timeline_match:test".to_owned());
        first_summary.target_context = first.target_context.clone();
        first_summary.direct_hp_evidence = true;
        let result = store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        assert_eq!(
            result.target_track_id.as_ref().map(|id| id.0.as_str()),
            Some("monster:boss_13#1")
        );

        let mut second = hit(1.2, 1_458_526.0, 1_450_000.0, 1_355_219.0);
        let mut second_summary = TargetResolutionSummary::default();
        let projected = store.attribute_damage_hit(
            &mut second,
            &mut second_summary,
            TrackPacketContext::default(),
        );
        assert!(projected.projected);
        assert_eq!(second.target_name.as_deref(), Some("无首铁驭"));
    }

    #[test]
    fn named_attribute_guid_hit_refreshes_existing_track() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1781881298.939,
            1_000_000.0,
            990_000.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "94b599e5291b934a8bef735771caa138",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );

        let (mut named, mut named_summary) = named_hit(
            1781881302.831,
            980_000.0,
            970_610.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "94b599e5291b934a8bef735771caa138",
        );
        named.target_id =
            Some("AttributeGuid:94b599e5291b934a8bef735771caa138|path=WorldBoss_Boss13".to_owned());
        named
            .target_context
            .push("reason=boss_hp_delta_match:9390".to_owned());
        named
            .target_context
            .push("reason=target_max_hp_only_weak".to_owned());
        named_summary.target_context = named.target_context.clone();
        named_summary.direct_hp_evidence = true;

        store.attribute_damage_hit(
            &mut named,
            &mut named_summary,
            TrackPacketContext::default(),
        );
        assert_eq!(named.target_id.as_deref(), Some("monster:boss_13#1"));
        let track = store
            .track(&TargetTrackId("monster:boss_13#1".to_owned()))
            .expect("track refreshed");
        assert_eq!(track.last_seen_at.to_bits(), 1781881302.831_f64.to_bits());
        assert_eq!(
            store
                .hp_handle_index
                .get("boss_hp_guid:94b599e5291b934a8bef735771caa138"),
            Some(&"monster:boss_13#1".to_owned())
        );
    }

    #[test]
    fn exact_boss_hp_guid_projects_even_when_continuity_window_expired() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            900.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "94b599e5291b934a8bef735771caa138",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );

        let mut late = hit(10.0, 850.0, 830.0, 1000.0);
        late.target_context
            .push("boss_hp_guid=94b599e5291b934a8bef735771caa138".to_owned());
        let mut late_summary = TargetResolutionSummary::default();
        let result =
            store.attribute_damage_hit(&mut late, &mut late_summary, TrackPacketContext::default());
        assert!(result.projected);
        assert_eq!(late.target_name.as_deref(), Some("无首铁驭"));
        assert_eq!(late.target_id.as_deref(), Some("monster:boss_13#1"));
        assert!(
            late.target_context
                .iter()
                .any(|entry| entry == "reason=unique_locked_hp_handle")
        );
        assert!(
            !late
                .target_context
                .iter()
                .any(|entry| entry == "track_reject_reason=hp_stream_mismatch")
        );
    }

    #[test]
    fn regression_timestamp_1781881303_978() {
        let mut store = TargetTrackStore::default();
        let (mut projected, mut projected_summary) = named_hit(
            1781881298.939,
            1_000_000.0,
            990_000.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "94b599e5291b934a8bef735771caa138",
        );
        store.attribute_damage_hit(
            &mut projected,
            &mut projected_summary,
            TrackPacketContext::default(),
        );
        let (mut named, mut named_summary) = named_hit(
            1781881302.831,
            980_000.0,
            970_610.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "94b599e5291b934a8bef735771caa138",
        );
        named.target_id =
            Some("AttributeGuid:94b599e5291b934a8bef735771caa138|path=WorldBoss_Boss13".to_owned());
        named
            .target_context
            .push("reason=boss_hp_delta_match:9390".to_owned());
        named_summary.target_context = named.target_context.clone();
        named_summary.direct_hp_evidence = true;
        store.attribute_damage_hit(
            &mut named,
            &mut named_summary,
            TrackPacketContext::default(),
        );

        let mut unknown = hit(1781881303.978, 970_610.0, 960_000.0, 980_000.0);
        unknown
            .target_context
            .push("boss_hp_guid=94b599e5291b934a8bef735771caa138".to_owned());
        unknown
            .target_context
            .push("reason=near_object_path:WorldBoss_Boss13".to_owned());
        unknown
            .target_context
            .push("reason=resolved_target_name_table".to_owned());
        let mut unknown_summary = TargetResolutionSummary::default();
        let result = store.attribute_damage_hit(
            &mut unknown,
            &mut unknown_summary,
            TrackPacketContext::default(),
        );
        assert!(result.projected);
        assert_eq!(unknown.target_name.as_deref(), Some("无首铁驭"));
        assert_eq!(unknown.target_id.as_deref(), Some("monster:boss_13#1"));
        assert!(
            unknown
                .target_context
                .iter()
                .any(|entry| entry == "track_decision=projected")
        );
        assert!(
            !unknown
                .target_context
                .iter()
                .any(|entry| entry.starts_with("track_reject_reason="))
        );
    }

    #[test]
    fn old_hits_are_never_rewritten_to_different_track_name() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            900.0,
            "A",
            "/Game/Monster/Boss_A.Boss_A_C",
            "a",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        let old_name = first.target_name.clone();
        let old_track =
            target_context_value(&first.target_context, "target_track_id").map(str::to_owned);
        let (mut conflict, mut conflict_summary) =
            named_hit(1.2, 900.0, 800.0, "B", "/Game/Monster/Boss_B.Boss_B_C", "a");
        store.attribute_damage_hit(
            &mut conflict,
            &mut conflict_summary,
            TrackPacketContext::default(),
        );

        assert_eq!(first.target_name, old_name);
        assert_eq!(
            target_context_value(&first.target_context, "target_track_id").map(str::to_owned),
            old_track
        );
        assert!(
            conflict
                .target_context
                .iter()
                .any(|entry| entry == "target_conflict=locked_path_mismatch")
        );
    }

    #[test]
    fn old_resolver_ambiguous_does_not_block_single_track_projection() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1500.0,
            1450.0,
            "无首铁驭",
            "/Game/Monster/Boss_Headless.Boss_Headless_C",
            "a",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );

        let mut unknown = hit(1.2, 1450.0, 1440.0, 1500.0);
        unknown
            .target_context
            .push("target_unresolved=ambiguous_multi_target".to_owned());
        unknown
            .target_context
            .push("target_suppressed=ambiguous_multi_target".to_owned());
        let mut summary = TargetResolutionSummary::default();
        let result =
            store.attribute_damage_hit(&mut unknown, &mut summary, TrackPacketContext::default());

        assert!(result.projected);
        assert_eq!(unknown.target_name.as_deref(), Some("无首铁驭"));
        assert!(
            !unknown
                .target_context
                .iter()
                .any(|entry| entry == "target_unresolved=ambiguous_multi_target")
        );
        assert!(
            unknown
                .target_context
                .iter()
                .any(|entry| { entry == "target_name_resolution=track_continuity_projected" })
        );
    }

    #[test]
    fn raw_multiple_path_candidates_same_target_do_not_block_projection() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            900.0,
            "A",
            "/Game/Monster/Boss_A.Boss_A_C",
            "a",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        let context = TrackPacketContext {
            canonical_target_paths: HashSet::from(["/Game/Monster/Boss_A.Boss_A_C".to_owned()]),
            canonical_target_names: HashSet::from(["A".to_owned()]),
            hp_handle_keys: HashSet::new(),
            has_multiple_canonical_targets: false,
        };
        let mut unknown = hit(1.2, 900.0, 850.0, 1000.0);
        let mut summary = TargetResolutionSummary::default();
        let result = store.attribute_damage_hit(&mut unknown, &mut summary, context);
        assert!(result.projected);
        assert_eq!(unknown.target_name.as_deref(), Some("A"));
    }

    #[test]
    fn multiple_hp_updates_same_track_do_not_block_projection() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            900.0,
            "A",
            "/Game/Monster/Boss_A.Boss_A_C",
            "a",
        );
        first.target_context.push("current_hp_token=b".to_owned());
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        let context = TrackPacketContext {
            hp_handle_keys: HashSet::from([
                "boss_hp_guid:a".to_owned(),
                "current_hp_token:b".to_owned(),
            ]),
            ..Default::default()
        };
        let mut unknown = hit(1.2, 900.0, 850.0, 1000.0);
        let mut summary = TargetResolutionSummary::default();
        let result = store.attribute_damage_hit(&mut unknown, &mut summary, context);
        assert!(result.projected);
        assert_eq!(unknown.target_name.as_deref(), Some("A"));
    }

    #[test]
    fn dot_damage_single_active_track_projects() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1_460_091.0,
            1_458_526.0,
            "无首铁驭",
            "/Game/Monster/Boss_Headless.Boss_Headless_C",
            "a",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        let mut dot = hit(1.1, 1_459_670.0, 1_459_249.0, 1_460_091.0);
        dot.damage_name = Some("安魂曲流血伤害L".to_owned());
        let mut summary = TargetResolutionSummary::default();
        let result =
            store.attribute_damage_hit(&mut dot, &mut summary, TrackPacketContext::default());
        assert!(result.projected);
        assert_eq!(dot.target_name.as_deref(), Some("无首铁驭"));
        assert!(
            dot.target_context
                .iter()
                .any(|entry| entry == "target_name_resolution=track_dot_projection")
        );
    }

    #[test]
    fn dot_damage_multi_target_remains_unknown() {
        let mut store = TargetTrackStore::default();
        let (mut first, mut first_summary) = named_hit(
            1.0,
            1000.0,
            900.0,
            "A",
            "/Game/Monster/Boss_A.Boss_A_C",
            "a",
        );
        let (mut second, mut second_summary) = named_hit(
            1.0,
            1000.0,
            910.0,
            "B",
            "/Game/Monster/Boss_B.Boss_B_C",
            "b",
        );
        store.attribute_damage_hit(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );
        store.attribute_damage_hit(
            &mut second,
            &mut second_summary,
            TrackPacketContext::default(),
        );
        let mut dot = hit(1.1, 910.0, 880.0, 1000.0);
        dot.damage_name = Some("安魂曲流血伤害L".to_owned());
        let mut summary = TargetResolutionSummary::default();
        let result =
            store.attribute_damage_hit(&mut dot, &mut summary, TrackPacketContext::default());
        assert!(result.ambiguous);
        assert_eq!(dot.target_name, None);
        assert!(
            dot.target_context
                .iter()
                .any(|entry| entry == "target_unresolved=ambiguous_multi_target")
        );
    }

    #[test]
    fn attribute_guid_and_class_path_merge_into_one_track() {
        let mut store = TargetTrackStore::default();
        let (mut hit1, mut summary1) = named_hit(
            1.0,
            1000.0,
            900.0,
            "无首铁驭",
            "WorldBoss_Boss13",
            "94b599e5291b934a8bef735771caa138",
        );
        hit1.target_id =
            Some("AttributeGuid:94b599e5291b934a8bef735771caa138|path=WorldBoss_Boss13".to_owned());
        store.attribute_damage_hit(&mut hit1, &mut summary1, TrackPacketContext::default());

        let (mut hit2, mut summary2) = named_hit(
            1.5,
            900.0,
            850.0,
            "无首铁驭",
            "/Game/Blueprints/Character/Monster/boss_13/boss_13_BP.boss_13_BP_C",
            "94b599e5291b934a8bef735771caa138",
        );
        store.attribute_damage_hit(&mut hit2, &mut summary2, TrackPacketContext::default());

        assert_eq!(hit1.target_id.as_deref(), Some("monster:boss_13#1"));
        assert_eq!(hit2.target_id.as_deref(), Some("monster:boss_13#1"));
        assert_eq!(
            target_context_value(&hit1.target_context, "target_track_id"),
            target_context_value(&hit2.target_context, "target_track_id")
        );
        assert!(
            !hit2
                .target_context
                .iter()
                .any(|entry| entry == "target_conflict=locked_path_mismatch")
        );
        let track = store
            .track(&TargetTrackId("monster:boss_13#1".to_owned()))
            .expect("merged track");
        assert_eq!(track.canonical_target_key, "monster:boss_13");
        assert!(track.observed_paths.contains("WorldBoss_Boss13"));
        assert!(
            track
                .observed_paths
                .contains("/Game/Blueprints/Character/Monster/boss_13/boss_13_BP.boss_13_BP_C")
        );
    }

    #[test]
    fn no_duplicate_generation_for_same_monster_alias_switch() {
        let mut store = TargetTrackStore::default();
        for (timestamp, path) in [
            (1.0, "WorldBoss_Boss13"),
            (
                1.4,
                "/Game/Blueprints/Character/Monster/boss_13/boss_13_BP.boss_13_BP_C",
            ),
            (1.8, "WorldBoss_Boss13"),
        ] {
            let (mut hit, mut summary) = named_hit(
                timestamp,
                1000.0 - timestamp * 10.0,
                990.0 - timestamp * 10.0,
                "无首铁驭",
                path,
                "94b599e5291b934a8bef735771caa138",
            );
            store.attribute_damage_hit(&mut hit, &mut summary, TrackPacketContext::default());
            assert_eq!(hit.target_id.as_deref(), Some("monster:boss_13#1"));
            assert!(
                !hit.target_context
                    .iter()
                    .any(|entry| entry == "target_conflict=locked_path_mismatch")
            );
        }
        let track = store
            .track(&TargetTrackId("monster:boss_13#1".to_owned()))
            .expect("merged track");
        assert_eq!(track.observed_paths.len(), 2);
    }

    #[test]
    fn screenshot_sequence_regression_projects_dot_rows() {
        let mut store = TargetTrackStore::default();
        let sequence = [
            (1.0, true, 1_460_091.0, 1_458_526.0),
            (1.1, false, 1_460_091.0, 1_459_670.0),
            (2.0, true, 1_459_249.0, 1_441_960.0),
            (2.1, false, 1_459_249.0, 1_438_552.0),
            (3.0, true, 1_455_875.0, 1_430_648.0),
            (3.1, false, 1_455_875.0, 1_427_852.0),
        ];
        for (timestamp, named, before, after) in sequence {
            if named {
                let (mut hit, mut summary) = named_hit(
                    timestamp,
                    before,
                    after,
                    "无首铁驭",
                    "/Game/Monster/Boss_Headless.Boss_Headless_C",
                    "a",
                );
                store.attribute_damage_hit(&mut hit, &mut summary, TrackPacketContext::default());
            } else {
                let mut hit = hit(timestamp, before, after, before);
                hit.damage_name = Some("安魂曲流血伤害L".to_owned());
                let mut summary = TargetResolutionSummary::default();
                let result = store.attribute_damage_hit(
                    &mut hit,
                    &mut summary,
                    TrackPacketContext::default(),
                );
                assert!(result.projected, "DOT row at {timestamp} should project");
                assert_eq!(hit.target_name.as_deref(), Some("无首铁驭"));
            }
        }
    }
}
