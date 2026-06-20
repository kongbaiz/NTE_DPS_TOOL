#![allow(dead_code)]

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::parser::find_data_file;
use crate::resource_index::ResourceIndex;

const ADV_VISION_FILES: [&str; 2] = [
    "data/DataTable/Vision/DT_AdvVision.json",
    "res/data/DataTable/Vision/DT_AdvVision.json",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvVisionTarget {
    pub advvision_id: String,
    pub scene_id: String,
    pub target_name: String,
    pub target_path: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdvVisionRow {
    pub id: String,
    pub vision_name: Option<String>,
    pub vision_code: Option<String>,
    pub spawn_monster_pool_ids: HashMap<String, String>,
    pub scene_data_ids: HashMap<String, String>,
    pub target_descriptions: HashMap<String, String>,
}

#[derive(Default)]
pub struct AdvVisionIndex {
    by_advvision_id: HashMap<String, AdvVisionRow>,
    scene_to_advvision: HashMap<String, String>,
    target_name_by_scene: HashMap<String, String>,
    monster_key_by_target_name: HashMap<String, String>,
}

impl AdvVisionIndex {
    pub fn load_default(resources: &ResourceIndex) -> Self {
        let mut index = Self::default();
        for relative in ADV_VISION_FILES {
            let Some(path) = find_data_file(Path::new(relative)) else {
                continue;
            };
            index.load_advvision_file(&path);
        }
        for name in ["玛门", "斑蝶", "无首铁驭", "庭院花房"] {
            if let Some(path) = resources.canonical_target_path_for_path(name).or_else(|| {
                let candidates = [
                    "Boss_017_BP_Abyss",
                    "Boss_08_BP_Abyss",
                    "Boss_07_BP_Abyss",
                    "Boss_015_BP_Abyss",
                ];
                candidates
                    .iter()
                    .find(|candidate| {
                        resources.resolved_name_for_path(candidate).as_deref() == Some(name)
                    })
                    .map(|value| (*value).to_owned())
            }) {
                index
                    .monster_key_by_target_name
                    .entry(name.to_owned())
                    .or_insert(path);
            }
        }
        index
    }

    pub fn resolve_scene(&self, advvision_id: &str, stage_key: &str) -> Option<AdvVisionTarget> {
        let row = self.by_advvision_id.get(advvision_id)?;
        let scene_id = row.scene_data_ids.get(stage_key)?;
        self.target_for_scene(scene_id)
    }

    pub fn target_for_scene(&self, scene_id: &str) -> Option<AdvVisionTarget> {
        let advvision_id = self.scene_to_advvision.get(scene_id)?;
        let target_name = self.target_name_by_scene.get(scene_id)?;
        Some(AdvVisionTarget {
            advvision_id: advvision_id.clone(),
            scene_id: scene_id.to_owned(),
            target_name: target_name.clone(),
            target_path: self.monster_key_by_target_name.get(target_name).cloned(),
        })
    }

    pub fn insert_monster_name_path_for_test(&mut self, name: &str, path: &str) {
        self.monster_key_by_target_name
            .insert(name.to_owned(), path.to_owned());
    }

    fn load_advvision_file(&mut self, path: &Path) {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(rows) = data_table_rows(&value) else {
            return;
        };
        for (id, row) in rows {
            let adv = AdvVisionRow {
                id: id.clone(),
                vision_name: localized_text(row.get("VisionName")),
                vision_code: localized_text(row.get("VisionCode")),
                spawn_monster_pool_ids: keyed_string_map(row.get("SpawnMonsterPoolIds")),
                scene_data_ids: keyed_string_map(row.get("SceneDataIds")),
                target_descriptions: keyed_text_map(row.get("TargetDescriptions")),
            };
            for (key, scene_id) in &adv.scene_data_ids {
                self.scene_to_advvision
                    .insert(scene_id.clone(), adv.id.clone());
                if let Some(description) = adv.target_descriptions.get(key)
                    && let Some(name) = target_name_from_description(description)
                {
                    self.target_name_by_scene.insert(scene_id.clone(), name);
                }
            }
            self.by_advvision_id.insert(adv.id.clone(), adv);
        }
    }
}

fn data_table_rows(value: &Value) -> Option<&Map<String, Value>> {
    match value {
        Value::Array(items) => items
            .first()
            .and_then(|item| item.get("Rows").or_else(|| item.get("rows")))
            .and_then(Value::as_object),
        Value::Object(object) => object
            .get("Rows")
            .or_else(|| object.get("rows"))
            .and_then(Value::as_object),
        _ => None,
    }
}

fn keyed_string_map(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some((
                entry.get("Key")?.as_str()?.to_owned(),
                entry.get("Value")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn keyed_text_map(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some((
                entry.get("Key")?.as_str()?.to_owned(),
                localized_text(entry.get("Value"))?,
            ))
        })
        .collect()
}

fn localized_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return non_empty(text);
    }
    let object = value.as_object()?;
    [
        "LocalizedString",
        "SourceString",
        "CultureInvariantString",
        "Key",
    ]
    .into_iter()
    .filter_map(|field| object.get(field).and_then(Value::as_str))
    .find_map(non_empty)
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value != "None").then(|| value.to_owned())
}

fn target_name_from_description(description: &str) -> Option<String> {
    let start = description.find('「')?;
    let end = description[start + '「'.len_utf8()..].find('」')?;
    let name = &description[start + '「'.len_utf8()..start + '「'.len_utf8() + end];
    non_empty(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_index::ResourceIndex;

    #[test]
    fn advvision_mammon_resolves_target_description_name() {
        let resources = ResourceIndex::load_default();
        let mut index = AdvVisionIndex::load_default(&resources);
        index.insert_monster_name_path_for_test("玛门", "Boss_017_BP_Abyss");

        let target = index
            .resolve_scene("AdvVision_Mammon", "3")
            .expect("mammon stage 3");

        assert_eq!(target.advvision_id, "AdvVision_Mammon");
        assert_eq!(target.scene_id, "AdvVision_Mammon_3");
        assert_eq!(target.target_name, "玛门");
        assert_eq!(target.target_path.as_deref(), Some("Boss_017_BP_Abyss"));
    }
}
