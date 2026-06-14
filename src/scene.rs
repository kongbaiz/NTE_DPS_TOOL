use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::model::SceneObservation;

const SCENE_INDEX_JSON: &str = include_str!("../res/data/scenes/scene_index.json");

#[derive(Debug, Deserialize)]
struct SceneIndexDocument {
    scenes: Vec<SceneDefinition>,
}

#[derive(Clone, Debug, Deserialize)]
struct SceneDefinition {
    id: String,
    display_name: String,
    category: String,
    priority: u8,
    tokens: Vec<String>,
}

#[derive(Default)]
struct SceneIndex {
    by_token: HashMap<String, SceneDefinition>,
}

impl SceneIndex {
    fn load() -> Self {
        let document: SceneIndexDocument =
            serde_json::from_str(SCENE_INDEX_JSON).expect("embedded scene index must be valid");
        let mut by_token = HashMap::new();
        for scene in document.scenes {
            for token in &scene.tokens {
                by_token.insert(token.clone(), scene.clone());
            }
        }
        Self { by_token }
    }

    fn detect(&self, timestamp: f64, decoded_text: &str) -> Vec<SceneObservation> {
        let mut candidates = decoded_text
            .lines()
            .map(str::trim)
            .filter(|value| value.starts_with("DataLayer_"))
            .filter_map(|token| self.by_token.get(token))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|scene| scene.priority);
        candidates.dedup_by(|left, right| left.id == right.id);

        let transition = candidates
            .iter()
            .find(|scene| scene.category == "transition")
            .copied();
        let primary = candidates
            .iter()
            .filter(|scene| scene.category != "transition")
            .max_by_key(|scene| scene.priority)
            .copied();
        transition
            .into_iter()
            .chain(primary)
            .map(|scene| SceneObservation {
                timestamp,
                id: scene.id.clone(),
                display_name: scene.display_name.clone(),
                category: scene.category.clone(),
                priority: scene.priority,
            })
            .collect()
    }
}

pub fn detect_scenes(timestamp: f64, decoded_text: &str) -> Vec<SceneObservation> {
    static INDEX: OnceLock<SceneIndex> = OnceLock::new();
    INDEX
        .get_or_init(SceneIndex::load)
        .detect(timestamp, decoded_text)
}

#[cfg(test)]
pub fn detect_scene(timestamp: f64, decoded_text: &str) -> Option<SceneObservation> {
    detect_scenes(timestamp, decoded_text).into_iter().last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SceneState;

    #[test]
    fn detects_data_layer() {
        let scene = detect_scene(1.0, "DataLayer_9FBCFE1E49B3D2C6196E538050975D52")
            .expect("abyss layer should be known");
        assert_eq!(scene.display_name, "深渊");
        assert_eq!(scene.id, "Abyss");
    }

    #[test]
    fn ignores_generated_cell_from_full_path() {
        assert!(
            detect_scene(
                1.0,
                "/Game/Maps/Map_bigworld/XL_map_bigworld_test/_Generated_/1CXIHESV8420RGID5HW15E9GO",
            )
            .is_none()
        );
    }

    #[test]
    fn lower_priority_scene_does_not_immediately_replace_combat_scene() {
        let mut state = SceneState::default();
        state.apply(SceneObservation {
            timestamp: 1.0,
            id: "battle".to_owned(),
            display_name: "战斗".to_owned(),
            category: "battle".to_owned(),
            priority: 100,
        });
        state.apply(SceneObservation {
            timestamp: 2.0,
            id: "activity".to_owned(),
            display_name: "活动".to_owned(),
            category: "anomaly".to_owned(),
            priority: 80,
        });
        assert_eq!(state.display_name(), "战斗");

        state.apply(SceneObservation {
            timestamp: 32.0,
            id: "activity".to_owned(),
            display_name: "活动".to_owned(),
            category: "anomaly".to_owned(),
            priority: 80,
        });
        assert_eq!(state.display_name(), "活动");
    }

    #[test]
    fn transition_layer_clears_previous_scene() {
        let mut state = SceneState::default();
        state.apply(SceneObservation {
            timestamp: 1.0,
            id: "battle".to_owned(),
            display_name: "战斗".to_owned(),
            category: "battle".to_owned(),
            priority: 100,
        });
        state.apply(SceneObservation {
            timestamp: 2.0,
            id: "parking".to_owned(),
            display_name: "场景切换中".to_owned(),
            category: "transition".to_owned(),
            priority: 255,
        });
        assert_eq!(state.display_name(), "战斗");
        state.apply(SceneObservation {
            timestamp: 2.1,
            id: "activity".to_owned(),
            display_name: "活动".to_owned(),
            category: "anomaly".to_owned(),
            priority: 80,
        });
        assert_eq!(state.display_name(), "活动");
    }
}
