use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::parser::find_data_file;

const ABYSS_MONSTER_STATIC_PATH: &str = "res/data/abyss/DT_MonsterStaticData_Abyss.json";
const MONSTER_PACK_DATA_PATH: &str = "res/data/abyss/DT_MonsterPackData.json";
const ABYSS_SEASON_NAMES_PATH: &str = "res/data/abyss/season_names_zh_cn.json";
const LEGACY_ABYSS_MONSTER_STATIC_PATH: &str =
    "NTE_Assets/DataTable/Monster/DT_MonsterStaticData_Abyss.json";
const LEGACY_MONSTER_PACK_DATA_PATH: &str = "NTE_Assets/DataTable/PackData/DT_MonsterPackData.json";
const LEGACY_ABYSS_LOCALIZATION_PATH: &str = "NTE_Assets/Localization/zh-CN/game.json";
type AbyssPackIdParts = (u32, u32, Option<u32>, Option<u32>, String);

#[derive(Clone, Debug, Default)]
pub struct AbyssMonsterDataset {
    pub seasons: Vec<AbyssSeason>,
}

#[derive(Clone, Debug)]
pub struct AbyssSeason {
    pub season: u32,
    pub name: Option<String>,
    pub floors: Vec<AbyssFloor>,
}

#[derive(Clone, Debug)]
pub struct AbyssFloor {
    pub season: u32,
    pub season_name: Option<String>,
    pub floor: u32,
    pub monsters: Vec<AbyssMonsterEntry>,
}

#[derive(Clone, Debug)]
pub struct AbyssMonsterEntry {
    pub pack_id: String,
    pub monster_id: String,
    pub name: String,
    pub half: Option<u32>,
    pub wave: Option<u32>,
    pub stats: AbyssMonsterStats,
}

#[derive(Clone, Debug, Default)]
pub struct AbyssMonsterStats {
    pub hp_max_base: f64,
    pub raw_props: Vec<(String, f64)>,
}

#[derive(Clone, Debug, Default)]
struct StaticMonsterInfo {
    name: Option<String>,
}

impl AbyssMonsterDataset {
    pub fn load() -> Result<Self> {
        let static_path = find_data_file(ABYSS_MONSTER_STATIC_PATH.as_ref())
            .or_else(|| find_data_file(LEGACY_ABYSS_MONSTER_STATIC_PATH.as_ref()))
            .with_context(|| format!("找不到深渊怪物静态表 {ABYSS_MONSTER_STATIC_PATH}"))?;
        let pack_path = find_data_file(MONSTER_PACK_DATA_PATH.as_ref())
            .or_else(|| find_data_file(LEGACY_MONSTER_PACK_DATA_PATH.as_ref()))
            .with_context(|| format!("找不到怪物数值表 {MONSTER_PACK_DATA_PATH}"))?;
        let static_rows = load_rows(&static_path)
            .with_context(|| format!("无法读取 {}", static_path.display()))?;
        let pack_rows =
            load_rows(&pack_path).with_context(|| format!("无法读取 {}", pack_path.display()))?;
        let static_index = build_static_index(&static_rows);
        let season_names = load_abyss_season_names();
        let mut floors = HashMap::<(u32, u32), Vec<AbyssMonsterEntry>>::new();

        for (pack_id, row) in &pack_rows {
            let Some((season, floor, half, wave, monster_id)) = parse_abyss_pack_id(pack_id) else {
                continue;
            };
            let stats = monster_stats(row);
            let static_info = lookup_static_monster(&static_index, &monster_id);
            let name = static_info
                .and_then(|info| info.name.clone())
                .unwrap_or_else(|| monster_id.clone());
            floors
                .entry((season, floor))
                .or_default()
                .push(AbyssMonsterEntry {
                    pack_id: pack_id.clone(),
                    monster_id,
                    name: name.clone(),
                    half,
                    wave,
                    stats,
                });
        }

        let mut grouped = HashMap::<u32, Vec<AbyssFloor>>::new();
        for ((season, floor), mut monsters) in floors {
            monsters.sort_by(|left, right| {
                left.half
                    .cmp(&right.half)
                    .then_with(|| left.wave.cmp(&right.wave))
                    .then_with(|| left.name.cmp(&right.name))
                    .then_with(|| left.pack_id.cmp(&right.pack_id))
            });
            let season_name = season_names.get(&season).cloned();
            grouped.entry(season).or_default().push(AbyssFloor {
                season,
                season_name,
                floor,
                monsters,
            });
        }

        let mut seasons = grouped
            .into_iter()
            .map(|(season, mut floors)| {
                floors.sort_by_key(|floor| floor.floor);
                AbyssSeason {
                    season,
                    name: season_names.get(&season).cloned(),
                    floors,
                }
            })
            .collect::<Vec<_>>();
        seasons.sort_by_key(|season| season.season);

        Ok(Self { seasons })
    }

    pub fn first_floor_key(&self) -> Option<(u32, u32)> {
        self.seasons
            .first()
            .and_then(|season| season.floors.first())
            .map(|floor| (floor.season, floor.floor))
    }

    pub fn season(&self, season: u32) -> Option<&AbyssSeason> {
        self.seasons.iter().find(|item| item.season == season)
    }

    pub fn floor(&self, season: u32, floor: u32) -> Option<&AbyssFloor> {
        self.season(season)
            .and_then(|season| season.floors.iter().find(|item| item.floor == floor))
    }

    pub fn monster(&self, pack_id: &str) -> Option<&AbyssMonsterEntry> {
        self.seasons
            .iter()
            .flat_map(|season| season.floors.iter())
            .flat_map(|floor| floor.monsters.iter())
            .find(|monster| monster.pack_id == pack_id)
    }
}

fn load_rows(path: &PathBuf) -> Result<HashMap<String, Value>> {
    let text = std::fs::read_to_string(path)?;
    let document: Value = serde_json::from_str(&text)?;
    let rows = document
        .as_array()
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("Rows"))
        .and_then(Value::as_object)
        .context("DataTable 缺少 Rows 对象")?;
    Ok(rows
        .iter()
        .map(|(key, row)| (key.clone(), row.clone()))
        .collect())
}

fn load_abyss_season_names() -> HashMap<u32, String> {
    let Some(path) = find_data_file(ABYSS_SEASON_NAMES_PATH.as_ref())
        .or_else(|| find_data_file(LEGACY_ABYSS_LOCALIZATION_PATH.as_ref()))
    else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(document) = serde_json::from_str::<Value>(&text) else {
        return HashMap::new();
    };
    parse_abyss_season_names(&document)
}

fn parse_abyss_season_names(document: &Value) -> HashMap<u32, String> {
    let mut names = parse_plain_abyss_season_names(document);
    collect_localized_abyss_season_names(document, &mut names);
    names
}

fn parse_plain_abyss_season_names(document: &Value) -> HashMap<u32, String> {
    let Some(object) = document.as_object() else {
        return HashMap::new();
    };
    object
        .iter()
        .filter_map(|(key, value)| {
            let season = key.parse::<u32>().ok()?;
            let name = valid_abyss_season_name(value)?;
            Some((season, name.to_owned()))
        })
        .collect()
}

fn collect_localized_abyss_season_names(document: &Value, names: &mut HashMap<u32, String>) {
    let Some(object) = document.as_object() else {
        return;
    };
    for (key, value) in object {
        if let Some(season_text) = key
            .strip_prefix("Abyss_")
            .and_then(|value| value.strip_suffix("_name"))
            && let Ok(season) = season_text.parse::<u32>()
            && let Some(name) = valid_abyss_season_name(value)
        {
            names.insert(season, name.to_owned());
        }
        collect_localized_abyss_season_names(value, names);
    }
}

fn valid_abyss_season_name(value: &Value) -> Option<&str> {
    let name = value.as_str()?.trim();
    (!name.is_empty() && !name.contains(',')).then_some(name)
}

fn build_static_index(rows: &HashMap<String, Value>) -> HashMap<String, StaticMonsterInfo> {
    let mut index = HashMap::new();
    for (key, row) in rows {
        let name = row
            .get("Comment")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                row.get("TextName")
                    .and_then(|value| value.get("CultureInvariantString"))
                    .and_then(Value::as_str)
            })
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned);
        let info = StaticMonsterInfo { name };
        for lookup_key in static_monster_lookup_keys(key, row) {
            index.entry(lookup_key).or_insert_with(|| info.clone());
        }
    }
    index
}

fn lookup_static_monster<'a>(
    index: &'a HashMap<String, StaticMonsterInfo>,
    monster_id: &str,
) -> Option<&'a StaticMonsterInfo> {
    monster_lookup_keys(monster_id)
        .into_iter()
        .find_map(|key| index.get(&key))
}

fn static_monster_lookup_keys(key: &str, row: &Value) -> Vec<String> {
    let mut keys = monster_lookup_keys(key);
    if let Some(tags) = row.get("Tags").and_then(Value::as_array) {
        for tag in tags.iter().filter_map(Value::as_str) {
            for key in monster_lookup_keys(tag) {
                push_unique_key(&mut keys, key);
            }
        }
    }
    keys
}

fn monster_lookup_keys(value: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let normalized = normalize_monster_key(value);
    push_unique_key(&mut keys, normalized.clone());
    push_unique_key(&mut keys, normalize_monster_numeric_key(&normalized));
    for suffix in ["_bp", "_bf", "_b"] {
        if let Some(trimmed) = normalized.strip_suffix(suffix) {
            push_unique_key(&mut keys, trimmed.to_owned());
            push_unique_key(&mut keys, normalize_monster_numeric_key(trimmed));
        }
    }
    keys
}

fn push_unique_key(keys: &mut Vec<String>, key: String) {
    if !keys.iter().any(|existing| existing == &key) {
        keys.push(key);
    }
}

fn normalize_monster_key(value: &str) -> String {
    value
        .trim_end_matches("_Abyss")
        .trim_end_matches("_abyss")
        .to_ascii_lowercase()
}

fn normalize_monster_numeric_key(value: &str) -> String {
    value
        .split('_')
        .map(|part| {
            part.parse::<u32>()
                .map(|number| number.to_string())
                .unwrap_or_else(|_| part.to_owned())
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn parse_abyss_pack_id(pack_id: &str) -> Option<AbyssPackIdParts> {
    let parts = pack_id.split('_').collect::<Vec<_>>();
    if parts.len() < 4 || parts.first().copied() != Some("Abyss") {
        return None;
    }
    let season = parts.get(1)?.parse::<u32>().ok()?;
    let floor = parts.get(2)?.parse::<u32>().ok()?;
    let (half, wave, monster_start) = if parts.len() >= 6 && is_u32(parts[3]) && is_u32(parts[4]) {
        (
            parts[3].parse::<u32>().ok(),
            parts[4].parse::<u32>().ok(),
            5,
        )
    } else {
        (None, None, 3)
    };
    let monster_id = parts.get(monster_start..)?.join("_");
    (!monster_id.is_empty()).then_some((season, floor, half, wave, monster_id))
}

fn is_u32(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|character| character.is_ascii_digit())
}

fn monster_stats(row: &Value) -> AbyssMonsterStats {
    let mut raw_props = row
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| value.as_f64().map(|number| (key.clone(), number)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    raw_props.sort_by(|left, right| left.0.cmp(&right.0));
    AbyssMonsterStats {
        hp_max_base: number(row, "HPMaxBase"),
        raw_props,
    }
}

fn number(row: &Value, key: &str) -> f64 {
    row.get(key).and_then(Value::as_f64).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        build_static_index, lookup_static_monster, parse_abyss_pack_id, parse_abyss_season_names,
    };
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn parses_simple_abyss_pack_id() {
        assert_eq!(
            parse_abyss_pack_id("Abyss_1_9_Boss_016_BP"),
            Some((1, 9, None, None, "Boss_016_BP".to_owned()))
        );
    }

    #[test]
    fn parses_half_and_wave_abyss_pack_id() {
        assert_eq!(
            parse_abyss_pack_id("Abyss_4_6_1_2_mon_03_BP"),
            Some((4, 6, Some(1), Some(2), "mon_03_BP".to_owned()))
        );
    }

    #[test]
    fn static_lookup_matches_tags_and_numeric_padding() {
        let rows = HashMap::from([(
            "mon_016_BP_Abyss".to_owned(),
            json!({
                "Comment": "贩卖机",
                "TextName": {"CultureInvariantString": null},
                "Tags": ["mon_16_BP_Abyss"]
            }),
        )]);
        let index = build_static_index(&rows);

        assert_eq!(
            lookup_static_monster(&index, "mon_16_BP").and_then(|info| info.name.as_deref()),
            Some("贩卖机")
        );
    }

    #[test]
    fn static_lookup_matches_bp_and_bf_suffix_variants() {
        let rows = HashMap::from([(
            "mon_35_BP_Red_Abyss".to_owned(),
            json!({
                "Comment": "红锡兵(近战)",
                "Tags": ["mon_35_Red_BP_Abyss"]
            }),
        )]);
        let index = build_static_index(&rows);

        assert_eq!(
            lookup_static_monster(&index, "mon_35_Red_BF").and_then(|info| info.name.as_deref()),
            Some("红锡兵(近战)")
        );
    }

    #[test]
    fn parses_localized_abyss_season_names() {
        let names = parse_abyss_season_names(&json!({
            "Abyss_4_name": "晦冥环线",
            "Abyss_5_name": "ST_AbyssBattle,Abyss_5_name",
            "Buff_Abyss_Phase_004_name": "无星之夜"
        }));

        assert_eq!(names.get(&4).map(String::as_str), Some("晦冥环线"));
        assert!(!names.contains_key(&5));
    }

    #[test]
    fn parses_plain_abyss_season_names_resource() {
        let names = parse_abyss_season_names(&json!({
            "4": "晦冥环线",
            "5": "晦冥环线"
        }));

        assert_eq!(names.get(&4).map(String::as_str), Some("晦冥环线"));
        assert_eq!(names.get(&5).map(String::as_str), Some("晦冥环线"));
    }
}
