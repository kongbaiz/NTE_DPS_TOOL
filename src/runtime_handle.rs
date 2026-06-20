use std::collections::HashMap;

use crate::target_resolver::TargetConfidence;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StageContext {
    pub kind: String,
    pub stage_key: Option<String>,
    pub half: Option<String>,
    pub runtime_guid: Option<String>,
    pub advvision_id: Option<String>,
    pub scene_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum HandleEvidenceKind {
    FirstSeenS2C,
    ClientEcho,
    BossHpUpdate,
    SdkTargetHpUpdate,
    FCharacterForNetSource,
    AbyssGamePlayData,
    TargetPathBinding,
    DamageHpAfterMatch,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HandleEvidence {
    pub timestamp: f64,
    pub kind: HandleEvidenceKind,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HpObservation {
    pub timestamp: f64,
    pub current_hp: f64,
    pub source: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct RuntimeHandleState {
    pub handle: String,
    pub first_seen_at: f64,
    pub first_seen_packet: Option<usize>,
    pub last_seen_at: f64,
    pub stage_context: Option<StageContext>,
    pub target_name: Option<String>,
    pub target_path: Option<String>,
    pub confidence: TargetConfidence,
    pub evidence: Vec<HandleEvidence>,
    pub hp_timeline: Vec<HpObservation>,
}

#[derive(Default)]
pub struct RuntimeHandleStore {
    handles: HashMap<String, RuntimeHandleState>,
}

impl RuntimeHandleStore {
    pub fn observe(
        &mut self,
        timestamp: f64,
        handle: impl Into<String>,
        kind: HandleEvidenceKind,
        detail: impl Into<String>,
    ) -> &RuntimeHandleState {
        let handle = normalize_handle(handle.into());
        let state = self
            .handles
            .entry(handle.clone())
            .or_insert_with(|| RuntimeHandleState {
                handle,
                first_seen_at: timestamp,
                first_seen_packet: None,
                last_seen_at: timestamp,
                stage_context: None,
                target_name: None,
                target_path: None,
                confidence: TargetConfidence::Possible,
                evidence: Vec::new(),
                hp_timeline: Vec::new(),
            });
        state.last_seen_at = timestamp;
        state.evidence.push(HandleEvidence {
            timestamp,
            kind,
            detail: detail.into(),
        });
        state
    }

    pub fn observe_hp(
        &mut self,
        timestamp: f64,
        handle: impl Into<String>,
        current_hp: f64,
        source: impl Into<String>,
    ) -> &RuntimeHandleState {
        let source = source.into();
        let handle = normalize_handle(handle.into());
        self.observe(
            timestamp,
            handle.clone(),
            if source.contains("boss") {
                HandleEvidenceKind::BossHpUpdate
            } else {
                HandleEvidenceKind::SdkTargetHpUpdate
            },
            source.clone(),
        );
        let state = self.handles.get_mut(&handle).expect("handle exists");
        state.hp_timeline.push(HpObservation {
            timestamp,
            current_hp,
            source,
        });
        state.confidence = max_confidence(state.confidence, TargetConfidence::Probable);
        state
    }

    #[allow(dead_code)]
    pub fn bind_target(
        &mut self,
        timestamp: f64,
        handle: impl Into<String>,
        target_path: impl Into<String>,
        target_name: impl Into<String>,
        confidence: TargetConfidence,
        detail: impl Into<String>,
    ) {
        let handle = normalize_handle(handle.into());
        self.observe(
            timestamp,
            handle.clone(),
            HandleEvidenceKind::TargetPathBinding,
            detail,
        );
        let state = self.handles.get_mut(&handle).expect("handle exists");
        state.target_path = Some(target_path.into());
        state.target_name = Some(target_name.into());
        state.confidence = max_confidence(state.confidence, confidence);
    }

    #[cfg(test)]
    pub fn get(&self, handle: &str) -> Option<&RuntimeHandleState> {
        self.handles.get(&normalize_handle(handle.to_owned()))
    }
}

fn normalize_handle(handle: String) -> String {
    handle.trim().to_ascii_lowercase()
}

fn max_confidence(left: TargetConfidence, right: TargetConfidence) -> TargetConfidence {
    if left.rank() >= right.rank() {
        left
    } else {
        right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_handle_records_hp_without_becoming_identity() {
        let mut store = RuntimeHandleStore::default();
        store.observe_hp(1.0, "ABCDEF", 1000.0, "boss_hp");
        let state = store.get("abcdef").expect("handle");

        assert_eq!(state.handle, "abcdef");
        assert_eq!(state.target_name, None);
        assert_eq!(state.hp_timeline.len(), 1);
        assert_eq!(state.confidence, TargetConfidence::Probable);
    }
}
