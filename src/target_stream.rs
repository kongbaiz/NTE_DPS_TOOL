use std::collections::HashMap;

use crate::class_hint::ClassHint;
use crate::runtime_handle::HpObservation;
use crate::target_resolver::TargetConfidence;

const TARGET_STREAM_DEAD_HP_THRESHOLD: f64 = 1.0;
const HP_CONTINUITY_ABSOLUTE_TOLERANCE: f64 = 2.0;
const HP_CONTINUITY_RELATIVE_TOLERANCE: f64 = 0.002;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassHintBinding {
    pub canonical_class: String,
    pub target_name: Option<String>,
    pub confidence: TargetConfidence,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct TargetStreamState {
    pub stream_id: String,
    pub base_handle: String,
    pub slot: Option<u32>,
    pub suffix_hex: Option<String>,
    pub first_seen_at: f64,
    pub last_seen_at: f64,
    pub generation: u32,
    pub hp_timeline: Vec<HpObservation>,
    pub class_hint: Option<ClassHintBinding>,
    pub target_name: Option<String>,
    pub target_path: Option<String>,
    pub confidence: TargetConfidence,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetStreamObservation {
    pub stream_id: String,
    pub base_handle: String,
    pub slot: Option<u32>,
    pub suffix_hex: Option<String>,
}

#[derive(Default)]
pub struct TargetStreamStore {
    streams: HashMap<String, TargetStreamState>,
    generation_by_base_slot: HashMap<(String, Option<u32>), u32>,
}

impl TargetStreamStore {
    pub fn observe_sdk_target(
        &mut self,
        timestamp: f64,
        token: &[u8],
        current_hp: f64,
        class_hints: &[ClassHint],
    ) -> Option<TargetStreamObservation> {
        let descriptor = TargetStreamDescriptor::from_token(token)?;
        let generation = self.generation_for_observation(timestamp, &descriptor, current_hp);
        let stream_id = stream_id(&descriptor.base_handle, descriptor.slot, generation);
        let state = self
            .streams
            .entry(stream_id.clone())
            .or_insert_with(|| TargetStreamState {
                stream_id: stream_id.clone(),
                base_handle: descriptor.base_handle.clone(),
                slot: descriptor.slot,
                suffix_hex: descriptor.suffix_hex.clone(),
                first_seen_at: timestamp,
                last_seen_at: timestamp,
                generation,
                hp_timeline: Vec::new(),
                class_hint: None,
                target_name: None,
                target_path: None,
                confidence: TargetConfidence::Possible,
                evidence: Vec::new(),
            });
        state.last_seen_at = timestamp;
        state.hp_timeline.push(HpObservation {
            timestamp,
            current_hp,
            source: "sdk_target_hp".to_owned(),
        });
        if current_hp <= TARGET_STREAM_DEAD_HP_THRESHOLD {
            state.evidence.push("stream_dead".to_owned());
        }
        if state.hp_timeline.len() >= 2 {
            state.confidence = max_confidence(state.confidence, TargetConfidence::Probable);
        }
        bind_class_hint_if_safe(state, class_hints);
        Some(TargetStreamObservation {
            stream_id,
            base_handle: descriptor.base_handle,
            slot: descriptor.slot,
            suffix_hex: descriptor.suffix_hex,
        })
    }

    pub fn stream(&self, stream_id: &str) -> Option<&TargetStreamState> {
        self.streams.get(stream_id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    fn generation_for_observation(
        &mut self,
        timestamp: f64,
        descriptor: &TargetStreamDescriptor,
        current_hp: f64,
    ) -> u32 {
        let key = (descriptor.base_handle.clone(), descriptor.slot);
        let generation = *self.generation_by_base_slot.entry(key.clone()).or_insert(1);
        let current_id = stream_id(&descriptor.base_handle, descriptor.slot, generation);
        let Some(existing) = self.streams.get(&current_id) else {
            return generation;
        };
        let recently_dead = existing
            .hp_timeline
            .last()
            .is_some_and(|last| last.current_hp <= TARGET_STREAM_DEAD_HP_THRESHOLD);
        if recently_dead && current_hp > TARGET_STREAM_DEAD_HP_THRESHOLD {
            let next = generation + 1;
            self.generation_by_base_slot.insert(key, next);
            return next;
        }
        if existing.last_seen_at <= timestamp
            && existing
                .hp_timeline
                .last()
                .is_some_and(|last| current_hp <= last.current_hp + hp_tolerance(last.current_hp))
        {
            return generation;
        }
        generation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TargetStreamDescriptor {
    base_handle: String,
    slot: Option<u32>,
    suffix_hex: Option<String>,
}

impl TargetStreamDescriptor {
    fn from_token(token: &[u8]) -> Option<Self> {
        if token.len() < 28 {
            return None;
        }
        let base_handle = hex::encode(&token[8..24]);
        if base_handle.chars().all(|character| character == '0') {
            return None;
        }
        let suffix = &token[24..28];
        let slot = u32::from_le_bytes(suffix.try_into().ok()?);
        let plausible_slot = (slot <= 4096).then_some(slot);
        Some(Self {
            base_handle,
            slot: plausible_slot,
            suffix_hex: Some(hex::encode(suffix)),
        })
    }
}

fn bind_class_hint_if_safe(state: &mut TargetStreamState, class_hints: &[ClassHint]) {
    if state.class_hint.is_some() {
        return;
    }
    let named = class_hints
        .iter()
        .filter(|hint| hint.target_name.is_some())
        .collect::<Vec<_>>();
    if named.is_empty() {
        return;
    }
    let mut distinct_classes = named
        .iter()
        .map(|hint| hint.canonical_class.as_str())
        .collect::<Vec<_>>();
    distinct_classes.sort_unstable();
    distinct_classes.dedup();
    if distinct_classes.len() != 1 {
        state.evidence.push(format!(
            "class_hint_candidates={}",
            distinct_classes.join(",")
        ));
        state.confidence = max_confidence(state.confidence, TargetConfidence::Possible);
        return;
    }
    let hint = named[0];
    state.class_hint = Some(ClassHintBinding {
        canonical_class: hint.canonical_class.clone(),
        target_name: hint.target_name.clone(),
        confidence: TargetConfidence::Probable,
    });
    state.target_name = hint.target_name.clone();
    state.target_path = Some(hint.canonical_class.clone());
    state.confidence = max_confidence(state.confidence, TargetConfidence::Probable);
    state
        .evidence
        .push(format!("class_hint_stream_order:{}", hint.canonical_class));
}

fn stream_id(base_handle: &str, slot: Option<u32>, generation: u32) -> String {
    match slot {
        Some(slot) => format!("target_stream:{base_handle}:slot:{slot}:gen:{generation}"),
        None => format!("target_stream:{base_handle}:slot:unknown:gen:{generation}"),
    }
}

fn hp_tolerance(scale: f64) -> f64 {
    HP_CONTINUITY_ABSOLUTE_TOLERANCE.max(scale.abs() * HP_CONTINUITY_RELATIVE_TOLERANCE)
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

    fn token(base: [u8; 16], slot: u32, prefix: u8) -> [u8; 40] {
        let mut token = [0_u8; 40];
        token[0] = prefix;
        token[8..24].copy_from_slice(&base);
        token[24..28].copy_from_slice(&slot.to_le_bytes());
        token
    }

    fn hint(timestamp: f64, class: &str, name: &str) -> ClassHint {
        ClassHint {
            timestamp,
            packet_index: 1,
            direction: crate::parser::PacketDirection::ServerToClient,
            raw_class: class.to_owned(),
            canonical_class: class.to_owned(),
            target_name: Some(name.to_owned()),
            source: crate::class_hint::ClassHintSource::PathCandidate,
            confidence: TargetConfidence::Possible,
        }
    }

    #[test]
    fn raw_token_prefix_changes_keep_same_stream_for_same_base_slot() {
        let base = [0x5b; 16];
        let mut store = TargetStreamStore::default();
        let first = store
            .observe_sdk_target(1.0, &token(base, 1, 0xaa), 46_347.0, &[])
            .expect("first");
        let second = store
            .observe_sdk_target(1.1, &token(base, 1, 0xbb), 43_155.0, &[])
            .expect("second");
        let third = store
            .observe_sdk_target(1.2, &token(base, 1, 0xcc), 36_988.0, &[])
            .expect("third");

        assert_eq!(first.stream_id, second.stream_id);
        assert_eq!(second.stream_id, third.stream_id);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn class_hint_order_binds_bigworld_streams() {
        let base = [0x5b; 16];
        let mut store = TargetStreamStore::default();
        let stream0 = store
            .observe_sdk_target(
                1.0,
                &token(base, 0, 1),
                10_000.0,
                &[hint(0.9, "mon_01_BP", "低语种")],
            )
            .expect("slot 0");
        let stream1 = store
            .observe_sdk_target(
                1.1,
                &token(base, 1, 1),
                10_000.0,
                &[hint(1.0, "mon_01_BP", "低语种")],
            )
            .expect("slot 1");
        store.observe_sdk_target(2.0, &token(base, 0, 1), 0.0, &[]);
        store.observe_sdk_target(2.0, &token(base, 1, 1), 0.0, &[]);
        let stream2 = store
            .observe_sdk_target(
                3.0,
                &token(base, 2, 1),
                12_000.0,
                &[hint(2.9, "mon_04_BP", "迷失种")],
            )
            .expect("slot 2");

        assert_eq!(
            store
                .stream(&stream0.stream_id)
                .and_then(|stream| stream.target_name.as_deref()),
            Some("低语种")
        );
        assert_eq!(
            store
                .stream(&stream1.stream_id)
                .and_then(|stream| stream.target_name.as_deref()),
            Some("低语种")
        );
        assert_eq!(
            store
                .stream(&stream2.stream_id)
                .and_then(|stream| stream.target_name.as_deref()),
            Some("迷失种")
        );
    }

    #[test]
    fn conflicting_class_hints_do_not_bind_stream() {
        let base = [0x5b; 16];
        let mut store = TargetStreamStore::default();
        let stream = store
            .observe_sdk_target(
                1.0,
                &token(base, 0, 1),
                10_000.0,
                &[
                    hint(0.9, "mon_029_BP", "唱片机附电灵"),
                    hint(0.95, "mon_01_BP", "低语种"),
                ],
            )
            .expect("stream");
        let stream = store.stream(&stream.stream_id).expect("stream");

        assert_eq!(stream.target_name, None);
        assert_eq!(stream.confidence, TargetConfidence::Possible);
        assert!(
            stream
                .evidence
                .iter()
                .any(|entry| entry == "class_hint_candidates=mon_01_BP,mon_029_BP")
        );
    }
}
