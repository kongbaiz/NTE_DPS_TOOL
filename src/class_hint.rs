use std::collections::VecDeque;

use crate::parser::PacketDirection;
use crate::resource_index::ResourceIndex;
use crate::target_resolver::TargetConfidence;

const MAX_CLASS_HINTS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum ClassHintSource {
    PathCandidate,
    GameplayEffect,
    TextMarker,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassHint {
    pub timestamp: f64,
    pub packet_index: usize,
    pub direction: PacketDirection,
    pub raw_class: String,
    pub canonical_class: String,
    pub target_name: Option<String>,
    pub source: ClassHintSource,
    pub confidence: TargetConfidence,
}

#[derive(Default)]
pub struct ClassHintStore {
    hints: VecDeque<ClassHint>,
}

impl ClassHintStore {
    pub fn observe_path(
        &mut self,
        timestamp: f64,
        packet_index: usize,
        direction: PacketDirection,
        raw_path: &str,
        resources: &ResourceIndex,
    ) -> Option<ClassHint> {
        let canonical_class = canonicalize_class_hint(raw_path)?;
        let target_name = resources
            .resolved_name_for_path(&canonical_class)
            .or_else(|| resources.resolved_name_for_path(raw_path));
        let hint = ClassHint {
            timestamp,
            packet_index,
            direction,
            raw_class: raw_path.to_owned(),
            canonical_class,
            target_name,
            source: ClassHintSource::PathCandidate,
            confidence: TargetConfidence::Possible,
        };
        self.hints.push_back(hint.clone());
        while self.hints.len() > MAX_CLASS_HINTS {
            self.hints.pop_front();
        }
        Some(hint)
    }

    pub fn hints_near(&self, timestamp: f64, before: f64, after: f64) -> Vec<ClassHint> {
        self.hints
            .iter()
            .filter(|hint| {
                timestamp >= hint.timestamp - after && timestamp - hint.timestamp <= before
            })
            .cloned()
            .collect()
    }
}

pub fn canonicalize_class_hint(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim_matches(|character: char| character == '\0' || character.is_control())
        .trim();
    if trimmed.is_empty() || !is_targetish_class_hint(trimmed) || is_excluded_class_hint(trimmed) {
        return None;
    }
    let mut value = trimmed
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .rsplit('.')
        .next()
        .unwrap_or(trimmed)
        .trim_matches('"')
        .trim_matches('\'')
        .to_owned();
    if let Some(stripped) = value.strip_prefix("Default__") {
        value = stripped.to_owned();
    }
    if let Some(stripped) = value.strip_suffix("_C") {
        value = stripped.to_owned();
    }
    if let Some((head, tail)) = value.rsplit_once("_C_")
        && tail.bytes().all(|byte| byte.is_ascii_digit())
    {
        value = head.to_owned();
    }
    if let Some((head, tail)) = value.rsplit_once('_')
        && tail.len() >= 6
        && tail.bytes().all(|byte| byte.is_ascii_digit())
    {
        value = head.to_owned();
    }
    (!value.is_empty()).then_some(value)
}

fn is_targetish_class_hint(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("mon_")
        || lower.contains("boss_")
        || lower.contains("worldboss")
        || lower.contains("/monster/")
        || lower.contains("/boss/")
        || lower.contains("enemy")
        || lower.contains("htcharacter")
}

fn is_excluded_class_hint(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "buff",
        "ability",
        "player",
        "map_bigworld/_generated_",
        "animation",
        "passiveeffect",
        "/ui/",
        "effect",
        "drop_",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_index::ResourceIndex;

    #[test]
    fn class_hint_normalizes_monster_variants() {
        assert_eq!(
            canonicalize_class_hint("mon_35_BP_Blue_Abyss_C_2146429087").as_deref(),
            Some("mon_35_BP_Blue_Abyss")
        );
        assert_eq!(
            canonicalize_class_hint("/Game/Monster/Boss_017_BP_Abyss.Boss_017_BP_Abyss_C")
                .as_deref(),
            Some("Boss_017_BP_Abyss")
        );
        assert_eq!(
            canonicalize_class_hint("/Game/UI/PlayerBuff").as_deref(),
            None
        );
    }

    #[test]
    fn class_hint_resolves_target_name_from_data_table() {
        let resources = ResourceIndex::load_default();
        let mut store = ClassHintStore::default();
        let hint = store
            .observe_path(
                1.0,
                1,
                PacketDirection::ServerToClient,
                "mon_01_BP",
                &resources,
            )
            .expect("hint");

        assert_eq!(hint.canonical_class, "mon_01_BP");
        assert_eq!(hint.target_name.as_deref(), Some("低语种"));
    }
}
