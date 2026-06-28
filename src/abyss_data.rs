use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::parser::find_data_file;
use crate::resource::read_resource_text;

const ABYSS_MONSTER_STATIC_PATH: &str = "res/data/abyss/DT_MonsterStaticData_Abyss.json";
const MONSTER_PACK_DATA_PATH: &str = "res/data/abyss/DT_MonsterPackData.json";
const ABYSS_MONSTER_DATASET_PATH: &str = "res/data/abyss/abyss_monsters.json";
const ABYSS_FLOOR_MONSTER_SUMMARY_PATH: &str = "res/data/abyss/abyss_floor_monster_summary.json";
const ABYSS_SEASON_NAMES_PATH: &str = "res/data/abyss/season_names_zh_cn.json";
const LEGACY_ABYSS_MONSTER_STATIC_PATH: &str =
    "NTE_Assets/DataTable/Monster/DT_MonsterStaticData_Abyss.json";
const LEGACY_MONSTER_PACK_DATA_PATH: &str = "NTE_Assets/DataTable/PackData/DT_MonsterPackData.json";
const LEGACY_ABYSS_LOCALIZATION_PATH: &str = "NTE_Assets/Localization/zh-CN/game.json";
type AbyssPackIdParts = (u32, u32, Option<u32>, Option<u32>, String);
const SUPPORTED_ABYSS_SEASONS: std::ops::RangeInclusive<u32> = 1..=7;

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
    pub name: Option<String>,
    pub monsters: Vec<AbyssMonsterEntry>,
}

#[derive(Clone, Debug)]
pub struct AbyssMonsterEntry {
    pub pack_id: String,
    pub attribute_id: String,
    pub monster_pool_id: Option<String>,
    pub monster_id: String,
    pub name: String,
    pub count: u32,
    pub level: Option<u32>,
    pub half: Option<u32>,
    pub wave: Option<u32>,
    pub is_boss: bool,
    pub stats: AbyssMonsterStats,
}

#[derive(Clone, Debug, Default)]
pub struct AbyssMonsterStats {
    pub hp_max_base: f64,
    pub raw_props: Vec<(String, f64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AbyssWaveHp {
    pub wave: Option<u32>,
    pub hp: f64,
    pub monster_count: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AbyssWaveClearPrediction {
    pub wave: Option<u32>,
    pub hp: f64,
    pub seconds: f64,
    pub cumulative_seconds: f64,
}

#[derive(Clone, Debug, Default)]
struct StaticMonsterInfo {
    name: Option<String>,
}

impl AbyssMonsterDataset {
    pub fn load() -> Result<Self> {
        if let Some(compact_path) = find_data_file(ABYSS_MONSTER_DATASET_PATH.as_ref()) {
            let dataset = load_compact_dataset(&compact_path)
                .with_context(|| format!("无法读取 {}", compact_path.display()))?;
            if !dataset.seasons.is_empty() {
                return Ok(dataset);
            }
        }

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
        if let Some(summary_path) = find_data_file(ABYSS_FLOOR_MONSTER_SUMMARY_PATH.as_ref()) {
            let summary_rows = load_summary_rows(&summary_path)
                .with_context(|| format!("无法读取 {}", summary_path.display()))?;
            if !summary_rows.is_empty() {
                return Ok(build_dataset_from_summary(
                    &summary_rows,
                    &pack_rows,
                    &static_index,
                    &season_names,
                ));
            }
        }

        Ok(build_dataset_from_pack_rows(
            &pack_rows,
            &static_index,
            &season_names,
        ))
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

impl AbyssFloor {
    pub fn monster_count(&self) -> u32 {
        self.monsters.iter().map(|monster| monster.count).sum()
    }

    pub fn wave_count(&self) -> usize {
        self.monsters
            .iter()
            .filter_map(|monster| monster.wave)
            .collect::<std::collections::HashSet<_>>()
            .len()
    }
}

pub fn abyss_monster_total_hp(monster: &AbyssMonsterEntry) -> f64 {
    monster.stats.hp_max_base * f64::from(monster.count)
}

pub fn abyss_line_hp_total<'a>(monsters: impl IntoIterator<Item = &'a AbyssMonsterEntry>) -> f64 {
    monsters.into_iter().map(abyss_monster_total_hp).sum()
}

pub fn line_hp_by_wave<'a>(
    monsters: impl IntoIterator<Item = &'a AbyssMonsterEntry>,
) -> Vec<AbyssWaveHp> {
    let mut waves = HashMap::<Option<u32>, AbyssWaveHp>::new();
    for monster in monsters {
        let entry = waves.entry(monster.wave).or_insert_with(|| AbyssWaveHp {
            wave: monster.wave,
            ..Default::default()
        });
        entry.hp += abyss_monster_total_hp(monster);
        entry.monster_count = entry.monster_count.saturating_add(monster.count);
    }
    let mut waves = waves.into_values().collect::<Vec<_>>();
    waves.sort_by(|left, right| match (left.wave, right.wave) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    waves
}

pub fn required_dps_for_target_time(line_hp: f64, target_seconds: f64) -> Option<f64> {
    (line_hp > 0.0 && target_seconds > 0.0).then(|| line_hp / target_seconds)
}

pub fn predict_wave_clear_times(
    waves: &[AbyssWaveHp],
    team_dps: f64,
) -> Vec<AbyssWaveClearPrediction> {
    if team_dps <= 0.0 {
        return Vec::new();
    }
    let mut cumulative_seconds = 0.0;
    waves
        .iter()
        .map(|wave| {
            let seconds = wave.hp / team_dps;
            cumulative_seconds += seconds;
            AbyssWaveClearPrediction {
                wave: wave.wave,
                hp: wave.hp,
                seconds,
                cumulative_seconds,
            }
        })
        .collect()
}

fn build_dataset_from_pack_rows(
    pack_rows: &HashMap<String, Value>,
    static_index: &HashMap<String, StaticMonsterInfo>,
    season_names: &HashMap<u32, String>,
) -> AbyssMonsterDataset {
    let mut floors = HashMap::<(u32, u32), Vec<AbyssMonsterEntry>>::new();

    for (pack_id, row) in pack_rows {
        let Some((season, floor, half, wave, monster_id)) = parse_abyss_pack_id(pack_id) else {
            continue;
        };
        let stats = monster_stats(row);
        let static_info = lookup_static_monster(static_index, &monster_id);
        let name = static_info
            .and_then(|info| info.name.clone())
            .unwrap_or_else(|| monster_id.clone());
        floors
            .entry((season, floor))
            .or_default()
            .push(AbyssMonsterEntry {
                pack_id: pack_id.clone(),
                attribute_id: pack_id.clone(),
                monster_pool_id: None,
                monster_id,
                name: name.clone(),
                count: 1,
                level: None,
                half,
                wave,
                is_boss: false,
                stats,
            });
    }

    build_dataset(floors, HashMap::new(), season_names)
}

fn build_dataset_from_summary(
    summary_rows: &[Value],
    pack_rows: &HashMap<String, Value>,
    static_index: &HashMap<String, StaticMonsterInfo>,
    season_names: &HashMap<u32, String>,
) -> AbyssMonsterDataset {
    let mut floors = HashMap::<(u32, u32), Vec<AbyssMonsterEntry>>::new();
    let mut floor_names = HashMap::<(u32, u32), String>::new();

    for row in summary_rows {
        let Some(abyss_key) = string(row, "abyss") else {
            continue;
        };
        let Some(season) = parse_abyss_group(abyss_key) else {
            continue;
        };
        let Some(floor) = u32_value(row, "level_id") else {
            continue;
        };
        if let Some(level_name) = string(row, "level_name").filter(|value| !value.trim().is_empty())
        {
            floor_names
                .entry((season, floor))
                .or_insert_with(|| level_name.to_owned());
        }
        let half = string(row, "route").and_then(parse_abyss_route_half);
        let wave = u32_value(row, "wave");
        let monster_pool_id = string(row, "monster_pool_id").map(str::to_owned);
        let Some(monsters) = row.get("monsters").and_then(Value::as_array) else {
            continue;
        };

        for (index, monster) in monsters.iter().enumerate() {
            let Some(attribute_id) = string(monster, "attribute_id") else {
                continue;
            };
            let monster_id = parse_abyss_attribute_monster_id(attribute_id)
                .or_else(|| {
                    monster_id_from_class_path(string(monster, "class").unwrap_or_default())
                })
                .unwrap_or_else(|| attribute_id.to_owned());
            let stats = pack_rows
                .get(attribute_id)
                .map(monster_stats)
                .unwrap_or_default();
            let static_info = lookup_static_monster(static_index, &monster_id);
            let name = string(monster, "name")
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| static_info.and_then(|info| info.name.clone()))
                .unwrap_or_else(|| monster_id.clone());
            let count = u32_value(monster, "count").unwrap_or(1).max(1);
            let pack_id = match &monster_pool_id {
                Some(pool) => format!(
                    "{pool}:{}:{}:{attribute_id}:{index}",
                    half.map_or_else(|| "-".to_owned(), |value| value.to_string()),
                    wave.map_or_else(|| "-".to_owned(), |value| value.to_string())
                ),
                None => attribute_id.to_owned(),
            };
            floors
                .entry((season, floor))
                .or_default()
                .push(AbyssMonsterEntry {
                    pack_id,
                    attribute_id: attribute_id.to_owned(),
                    monster_pool_id: monster_pool_id.clone(),
                    monster_id,
                    name,
                    count,
                    level: u32_value(monster, "level"),
                    half,
                    wave,
                    is_boss: bool_value(monster, "is_boss"),
                    stats,
                });
        }
    }

    build_dataset(floors, floor_names, season_names)
}

fn build_dataset(
    floors: HashMap<(u32, u32), Vec<AbyssMonsterEntry>>,
    floor_names: HashMap<(u32, u32), String>,
    season_names: &HashMap<u32, String>,
) -> AbyssMonsterDataset {
    let mut grouped = HashMap::<u32, Vec<AbyssFloor>>::new();
    for ((season, floor), mut monsters) in floors {
        if !is_supported_abyss_season(season) {
            continue;
        }
        monsters.sort_by(|left, right| {
            left.half
                .cmp(&right.half)
                .then_with(|| left.wave.cmp(&right.wave))
                .then_with(|| left.monster_pool_id.cmp(&right.monster_pool_id))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.pack_id.cmp(&right.pack_id))
        });
        let season_name = season_names
            .get(&season)
            .cloned()
            .or_else(|| (season == 0).then(|| "通用配置".to_owned()));
        let name = floor_names.get(&(season, floor)).cloned();
        grouped.entry(season).or_default().push(AbyssFloor {
            season,
            season_name,
            floor,
            name,
            monsters,
        });
    }

    let mut seasons = grouped
        .into_iter()
        .map(|(season, mut floors)| {
            floors.sort_by_key(|floor| floor.floor);
            AbyssSeason {
                season,
                name: season_names
                    .get(&season)
                    .cloned()
                    .or_else(|| (season == 0).then(|| "通用配置".to_owned())),
                floors,
            }
        })
        .collect::<Vec<_>>();
    seasons.sort_by_key(|season| season.season);

    AbyssMonsterDataset { seasons }
}

fn is_supported_abyss_season(season: u32) -> bool {
    SUPPORTED_ABYSS_SEASONS.contains(&season)
}

fn load_rows(path: &Path) -> Result<HashMap<String, Value>> {
    let text = read_resource_text(path)?;
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

fn load_summary_rows(path: &Path) -> Result<Vec<Value>> {
    let text = read_resource_text(path)?;
    let document: Value = serde_json::from_str(&text)?;
    let rows = document
        .get("rows")
        .and_then(Value::as_array)
        .context("深渊怪物汇总缺少 rows 数组")?;
    Ok(rows.clone())
}

fn load_compact_dataset(path: &Path) -> Result<AbyssMonsterDataset> {
    let text = read_resource_text(path)?;
    let document: Value = serde_json::from_str(&text)?;
    let seasons = document
        .get("seasons")
        .and_then(Value::as_array)
        .context("深渊怪物数据缺少 seasons 数组")?;
    let mut parsed_seasons = Vec::new();
    for season_row in seasons {
        let Some(season) = u32_value(season_row, "season") else {
            continue;
        };
        if !is_supported_abyss_season(season) {
            continue;
        }
        let season_name = string(season_row, "name").map(str::to_owned);
        let mut floors = Vec::new();
        let Some(floor_rows) = season_row.get("floors").and_then(Value::as_array) else {
            continue;
        };
        for floor_row in floor_rows {
            let Some(floor) = u32_value(floor_row, "floor") else {
                continue;
            };
            let mut monsters = Vec::new();
            let Some(monster_rows) = floor_row.get("monsters").and_then(Value::as_array) else {
                continue;
            };
            for monster_row in monster_rows {
                let Some(pack_id) = string(monster_row, "pack_id") else {
                    continue;
                };
                let Some(monster_id) = string(monster_row, "monster_id") else {
                    continue;
                };
                monsters.push(AbyssMonsterEntry {
                    pack_id: pack_id.to_owned(),
                    attribute_id: string(monster_row, "attribute_id")
                        .unwrap_or(pack_id)
                        .to_owned(),
                    monster_pool_id: string(monster_row, "monster_pool_id").map(str::to_owned),
                    monster_id: monster_id.to_owned(),
                    name: string(monster_row, "name")
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or(monster_id)
                        .to_owned(),
                    count: u32_value(monster_row, "count").unwrap_or(1).max(1),
                    level: u32_value(monster_row, "level"),
                    half: u32_value(monster_row, "half"),
                    wave: u32_value(monster_row, "wave"),
                    is_boss: bool_value(monster_row, "is_boss"),
                    stats: monster_row
                        .get("stats")
                        .map(monster_stats)
                        .unwrap_or_default(),
                });
            }
            monsters.sort_by(|left, right| {
                left.half
                    .cmp(&right.half)
                    .then_with(|| left.wave.cmp(&right.wave))
                    .then_with(|| left.monster_pool_id.cmp(&right.monster_pool_id))
                    .then_with(|| left.name.cmp(&right.name))
                    .then_with(|| left.pack_id.cmp(&right.pack_id))
            });
            floors.push(AbyssFloor {
                season,
                season_name: season_name.clone(),
                floor,
                name: string(floor_row, "name").map(str::to_owned),
                monsters,
            });
        }
        floors.sort_by_key(|floor| floor.floor);
        parsed_seasons.push(AbyssSeason {
            season,
            name: season_name,
            floors,
        });
    }
    parsed_seasons.sort_by_key(|season| season.season);
    Ok(AbyssMonsterDataset {
        seasons: parsed_seasons,
    })
}

fn load_abyss_season_names() -> HashMap<u32, String> {
    let Some(path) = find_data_file(ABYSS_SEASON_NAMES_PATH.as_ref())
        .or_else(|| find_data_file(LEGACY_ABYSS_LOCALIZATION_PATH.as_ref()))
    else {
        return HashMap::new();
    };
    let Ok(text) = read_resource_text(&path) else {
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

fn parse_abyss_group(value: &str) -> Option<u32> {
    value.strip_prefix("Abyss_").and_then(|suffix| {
        if suffix == "Common" {
            Some(0)
        } else {
            suffix.parse::<u32>().ok()
        }
    })
}

fn parse_abyss_route_half(value: &str) -> Option<u32> {
    if value.ends_with("FirstHalf") {
        Some(0)
    } else if value.ends_with("SecondHalf") {
        Some(1)
    } else {
        None
    }
}

fn parse_abyss_attribute_monster_id(attribute_id: &str) -> Option<String> {
    let parts = attribute_id.split('_').collect::<Vec<_>>();
    if parts.len() < 4 || parts.first().copied() != Some("Abyss") {
        return None;
    }
    let monster_start = if parts.get(1).copied() == Some("Common") {
        3
    } else if parts.get(1).is_some_and(|value| is_u32(value))
        && parts.get(2).is_some_and(|value| is_u32(value))
        && parts.get(3).is_some_and(|value| is_u32(value))
        && parts.get(4).is_some_and(|value| is_u32(value))
    {
        5
    } else if parts.get(1).is_some_and(|value| is_u32(value))
        && parts.get(2).is_some_and(|value| is_u32(value))
    {
        3
    } else {
        return None;
    };
    let monster_id = parts.get(monster_start..)?.join("_");
    (!monster_id.is_empty()).then_some(monster_id)
}

fn monster_id_from_class_path(path: &str) -> Option<String> {
    let asset_name = path
        .rsplit_once('/')
        .map(|(_, name)| name)
        .unwrap_or(path)
        .split('.')
        .next()
        .unwrap_or(path);
    let normalized = asset_name
        .trim_end_matches("_C")
        .trim_end_matches("_Abyss")
        .trim_end_matches("_abyss");
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

fn is_u32(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|character| character.is_ascii_digit())
}

fn string<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn u32_value(row: &Value, key: &str) -> Option<u32> {
    row.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn bool_value(row: &Value, key: &str) -> bool {
    row.get(key).and_then(Value::as_bool).unwrap_or(false)
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
        build_dataset_from_summary, build_static_index, lookup_static_monster,
        parse_abyss_attribute_monster_id, parse_abyss_group, parse_abyss_pack_id,
        parse_abyss_route_half, parse_abyss_season_names,
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
    fn parses_summary_abyss_identifiers() {
        assert_eq!(parse_abyss_group("Abyss_Common"), Some(0));
        assert_eq!(parse_abyss_group("Abyss_4"), Some(4));
        assert_eq!(
            parse_abyss_route_half("EAbyssFightStage::FirstHalf"),
            Some(0)
        );
        assert_eq!(
            parse_abyss_route_half("EAbyssFightStage::SecondHalf"),
            Some(1)
        );
        assert_eq!(
            parse_abyss_attribute_monster_id("Abyss_Common_2_mon_35_Blue_BP").as_deref(),
            Some("mon_35_Blue_BP")
        );
        assert_eq!(
            parse_abyss_attribute_monster_id("Abyss_4_6_1_2_mon_03_BP").as_deref(),
            Some("mon_03_BP")
        );
    }

    #[test]
    fn builds_summary_dataset_with_counts_and_stats() {
        let summary_rows = vec![json!({
            "abyss": "Abyss_4",
            "level_id": 2,
            "level_name": "第二站",
            "route": "EAbyssFightStage::FirstHalf",
            "wave": 1,
            "monster_pool_id": "Abyss_4_2_0_1",
            "monsters": [
                {
                    "name": "罐头锡兵",
                    "class": "/Game/Blueprints/Character/Monster/mon_35/Abyss/mon_35_BP_Blue_Abyss.mon_35_BP_Blue_Abyss_C",
                    "count": 2,
                    "level": 46,
                    "attribute_id": "Abyss_4_2_0_1_mon_35_Blue_BP",
                    "element_types": ["CHARACTER_ELEMENT_TYPE_COSMOS"],
                    "is_boss": false,
                    "spawn_points": ["MonPoint_01", "MonPoint_02"]
                }
            ]
        })];
        let pack_rows = HashMap::from([(
            "Abyss_4_2_0_1_mon_35_Blue_BP".to_owned(),
            json!({
                "HPMaxBase": 1000.0,
                "AttackBase": 50.0
            }),
        )]);
        let static_rows = HashMap::from([(
            "mon_35_BP_Blue_Abyss".to_owned(),
            json!({
                "Comment": "蓝锡兵",
                "Tags": ["mon_35_Blue_BP_Abyss"]
            }),
        )]);
        let static_index = build_static_index(&static_rows);
        let dataset =
            build_dataset_from_summary(&summary_rows, &pack_rows, &static_index, &HashMap::new());
        let floor = dataset.floor(4, 2).expect("summary floor should exist");
        assert_eq!(floor.name.as_deref(), Some("第二站"));
        assert_eq!(floor.monster_count(), 2);
        assert_eq!(floor.wave_count(), 1);

        let monster = floor
            .monsters
            .first()
            .expect("summary monster should exist");
        assert_eq!(monster.count, 2);
        assert_eq!(monster.level, Some(46));
        assert_eq!(monster.half, Some(0));
        assert_eq!(monster.wave, Some(1));
        assert_eq!(monster.monster_id, "mon_35_Blue_BP");
        assert_eq!(monster.stats.hp_max_base, 1000.0);
    }

    #[test]
    fn loads_current_abyss_summary_resource_shape() {
        let dataset = super::AbyssMonsterDataset::load().expect("abyss resource should load");
        let seasons = dataset
            .seasons
            .iter()
            .map(|season| season.season)
            .collect::<Vec<_>>();
        assert_eq!(seasons, [1, 2, 3, 4, 5, 6, 7]);

        let floor = dataset
            .floor(4, 1)
            .expect("current abyss floor should exist");
        assert_eq!(floor.wave_count(), 2);
        assert_eq!(floor.monster_count(), 16);
        assert!(
            floor
                .monsters
                .iter()
                .any(|monster| monster.monster_id == "mon_14_BP")
        );

        let latest_floor = dataset
            .floor(7, 1)
            .expect("latest abyss floor should exist");
        assert_eq!(latest_floor.wave_count(), 2);
        assert_eq!(latest_floor.monster_count(), 4);
    }

    #[test]
    fn loads_compact_abyss_dataset_shape() {
        let path = std::env::temp_dir().join(format!(
            "nte_dps_tool_compact_abyss_{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{
                "seasons": [{
                    "season": 4,
                    "name": "测试赛季",
                    "floors": [{
                        "floor": 2,
                        "name": "第二站",
                        "monsters": [{
                            "pack_id": "Abyss_4_2_0_1:0:1:Abyss_4_2_0_1_mon_35_Blue_BP:0",
                            "attribute_id": "Abyss_4_2_0_1_mon_35_Blue_BP",
                            "monster_pool_id": "Abyss_4_2_0_1",
                            "monster_id": "mon_35_Blue_BP",
                            "name": "罐头锡兵",
                            "count": 2,
                            "level": 46,
                            "half": 0,
                            "wave": 1,
                            "stats": {
                                "HPMaxBase": 1000.0,
                                "AttackBase": 50.0
                            }
                        }]
                    }]
                }]
            }"#,
        )
        .expect("compact abyss temp file should be writable");

        let dataset = super::load_compact_dataset(&path).expect("compact abyss should load");
        let floor = dataset.floor(4, 2).expect("compact floor should exist");
        let monster = floor
            .monsters
            .first()
            .expect("compact monster should exist");

        assert_eq!(floor.name.as_deref(), Some("第二站"));
        assert_eq!(monster.monster_id, "mon_35_Blue_BP");
        assert_eq!(monster.count, 2);
        assert_eq!(monster.stats.hp_max_base, 1000.0);
        assert!(
            monster
                .stats
                .raw_props
                .iter()
                .any(|(key, value)| key == "AttackBase" && *value == 50.0)
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

    #[test]
    fn predicts_wave_clear_times_from_static_hp() {
        let monsters = vec![
            super::AbyssMonsterEntry {
                pack_id: "a".to_owned(),
                attribute_id: "a".to_owned(),
                monster_pool_id: None,
                monster_id: "m1".to_owned(),
                name: "一号".to_owned(),
                count: 2,
                level: None,
                half: Some(0),
                wave: Some(1),
                is_boss: false,
                stats: super::AbyssMonsterStats {
                    hp_max_base: 100.0,
                    raw_props: Vec::new(),
                },
            },
            super::AbyssMonsterEntry {
                pack_id: "b".to_owned(),
                attribute_id: "b".to_owned(),
                monster_pool_id: None,
                monster_id: "m2".to_owned(),
                name: "二号".to_owned(),
                count: 1,
                level: None,
                half: Some(0),
                wave: Some(2),
                is_boss: false,
                stats: super::AbyssMonsterStats {
                    hp_max_base: 300.0,
                    raw_props: Vec::new(),
                },
            },
        ];

        let waves = super::line_hp_by_wave(&monsters);
        let predictions = super::predict_wave_clear_times(&waves, 100.0);

        assert_eq!(super::abyss_line_hp_total(&monsters), 500.0);
        assert_eq!(super::required_dps_for_target_time(500.0, 10.0), Some(50.0));
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0].hp, 200.0);
        assert_eq!(predictions[0].seconds, 2.0);
        assert_eq!(predictions[1].cumulative_seconds, 5.0);
    }
}
