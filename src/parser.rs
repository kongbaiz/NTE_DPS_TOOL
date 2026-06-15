use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::{CharacterInfo, Hit};

const RECORD_FIELD_TYPES: [u8; 10] = [12, 12, 12, 13, 12, 12, 6, 6, 6, 12];
const RECORD_FIELD_LENGTHS: [usize; 10] = [4, 4, 4, 8, 4, 4, 4, 4, 4, 4];
const MAX_RECORD_FIELD_LENGTH: usize = 8;
const MIN_DAMAGE: f32 = 2.0;
const MAX_DAMAGE: f32 = 1_000_000_000.0;
const MAX_PLAUSIBLE_CHARACTER_HP: f32 = 500_000.0;
const CURRENT_HP_PREFIX_LENGTH: usize = 16;
const BOSS_HP_PREFIX_LENGTH: usize = 36;
const BOSS_HP_PREFIX_HEAD: [u8; 8] = [0x06, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00];
const ACTIVE_GAMEPLAY_EFFECT_ANCHOR: &[u8] = b"FHTClientActiveGE";
const ACTIVE_GAMEPLAY_EFFECT_VALUE_OFFSET: usize = 5;
const ACTIVE_GAMEPLAY_EFFECT_MARKER: u32 = 12;

pub const CHARACTER_DATA_PATH: &str = "res/data/characters/characters.json";
pub const GAMEPLAY_EFFECT_MAPPING_PATH: &str = "res/data/skills/gameplay_effect_mapping.json";
pub const SKILL_DAMAGE_DATA_PATH: &str = "res/data/skills/skill_damage.json";
pub const WOODEN_DAMAGE_DESCRIPTIONS_PATH: &str = "res/data/skills/wooden_damage_descriptions.json";
const MONSTER_INDEX_JSON: &str = include_str!("../res/data/monsters/monster_index.json");

#[derive(Debug, Deserialize)]
struct MonsterIndexDocument {
    #[serde(default)]
    icons: HashMap<String, String>,
    aliases: HashMap<String, MonsterIndexEntry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct MonsterIndexEntry {
    id: String,
    display_name: String,
}

fn monster_document() -> &'static MonsterIndexDocument {
    static INDEX: OnceLock<MonsterIndexDocument> = OnceLock::new();
    INDEX.get_or_init(|| {
        serde_json::from_str(MONSTER_INDEX_JSON).expect("embedded monster index must be valid")
    })
}

fn monster_index() -> &'static HashMap<String, MonsterIndexEntry> {
    &monster_document().aliases
}

pub fn monster_icon_paths() -> impl Iterator<Item = &'static str> {
    monster_document().icons.values().map(String::as_str)
}

pub fn monster_icon_path(
    target_id: Option<&str>,
    target_name: Option<&str>,
) -> Option<&'static str> {
    if let Some(target_id) = target_id
        && let Some(path) = match_monster_icon(target_id)
    {
        return Some(path);
    }
    let target_name = target_name?;
    monster_index()
        .iter()
        .filter(|(_, monster)| monster.display_name == target_name)
        .filter_map(|(alias, monster)| {
            match_monster_icon(alias).or_else(|| match_monster_icon(&monster.id))
        })
        .next()
}

fn match_monster_icon(value: &str) -> Option<&'static str> {
    let normalized = value
        .strip_suffix("_C")
        .or_else(|| value.strip_suffix("_c"))
        .unwrap_or(value)
        .to_ascii_lowercase();
    monster_document()
        .icons
        .iter()
        .filter_map(|(family, path)| {
            monster_icon_match_score(&normalized, family).map(|score| (score, path.as_str()))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, path)| path)
}

fn monster_icon_match_score(value: &str, family: &str) -> Option<usize> {
    if value == family {
        return Some(10_000 + family.len());
    }
    if value.starts_with(&format!("{family}_")) {
        return Some(3_000 + family.split('_').count() * 100 + family.len());
    }

    let (value_kind, value_number) = monster_family_number(value)?;
    let (family_kind, family_number) = monster_family_number(family)?;
    if value_kind != family_kind || value_number != family_number {
        return None;
    }
    let suffix_matches = family
        .split('_')
        .skip(2)
        .filter(|part| !part.is_empty() && value.split('_').any(|value_part| value_part == *part))
        .count();
    Some(4_000 + suffix_matches * 100 + family.len())
}

fn monster_family_number(value: &str) -> Option<(&str, u32)> {
    let (kind, tail) = value.split_once('_')?;
    if !matches!(kind, "boss" | "mon") {
        return None;
    }
    let digits = tail
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    Some((kind, digits.parse().ok()?))
}

#[derive(Deserialize)]
struct CharacterDocument {
    characters: HashMap<String, CharacterInfo>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Field {
    raw: [u8; MAX_RECORD_FIELD_LENGTH],
    len: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedDamageRecord {
    pub damage: f32,
    pub target_hp_before: f32,
    pub target_max_hp: f32,
    pub damage_time: f64,
    pub world_time: f32,
    pub repeated_damage: f32,
    pub state_flags: [i32; 3],
    pub trailing_value: f32,
    pub byte_offset: usize,
    pub bit_shift: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedCurrentHpUpdate {
    pub current_hp: f32,
    pub byte_offset: usize,
    pub bit_shift: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedBossHpUpdate {
    pub target_handle: [u8; 16],
    pub current_hp: f32,
    pub byte_offset: usize,
    pub bit_shift: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedGameplayEffect {
    pub unique_index: u32,
    pub byte_offset: usize,
    pub bit_shift: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameplayEffectSkill {
    pub damage_source_category: Option<String>,
    pub ability_name: Option<String>,
    pub attack_type: String,
}

pub fn find_data_file(relative_path: &Path) -> Option<PathBuf> {
    if relative_path.is_file() {
        return Some(relative_path.to_path_buf());
    }

    if let Ok(current_dir) = std::env::current_dir() {
        let candidate = current_dir.join(relative_path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    if let Ok(executable) = std::env::current_exe() {
        for ancestor in executable.ancestors().skip(1) {
            let candidate = ancestor.join(relative_path);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let manifest_candidate = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path);
    manifest_candidate.is_file().then_some(manifest_candidate)
}

pub fn load_characters(path: &Path) -> Result<HashMap<u32, CharacterInfo>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("无法读取角色表 {}", path.display()))?;
    let document: CharacterDocument = serde_json::from_str(&text).context("角色表 JSON 无效")?;
    Ok(document
        .characters
        .into_iter()
        .filter_map(|(key, value)| key.parse::<u32>().ok().map(|id| (id, value)))
        .collect())
}

pub fn load_gameplay_effect_mapping(path: &Path) -> Result<HashMap<u32, String>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("无法读取 GameplayEffect 映射表 {}", path.display()))?;
    let document: serde_json::Value =
        serde_json::from_str(&text).context("GameplayEffect 映射表 JSON 无效")?;
    let rows = document
        .as_array()
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("Rows"))
        .and_then(serde_json::Value::as_object)
        .context("GameplayEffect 映射表缺少 Rows")?;

    Ok(rows
        .iter()
        .filter_map(|(name, row)| {
            row.get("UniqueIndex")
                .and_then(serde_json::Value::as_u64)
                .and_then(|index| u32::try_from(index).ok())
                .filter(|index| *index != 0)
                .map(|index| (index, name.clone()))
        })
        .collect())
}

pub fn load_gameplay_effect_skills(path: &Path) -> Result<HashMap<String, GameplayEffectSkill>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("无法读取技能伤害表 {}", path.display()))?;
    let document: serde_json::Value =
        serde_json::from_str(&text).context("技能伤害表 JSON 无效")?;
    let rows = document
        .as_array()
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("Rows"))
        .and_then(serde_json::Value::as_object)
        .context("技能伤害表缺少 Rows")?;

    Ok(rows
        .iter()
        .map(|(effect_name, row)| {
            let category = row
                .get("DamageSourceCategory")
                .and_then(serde_json::Value::as_str)
                .and_then(damage_source_category_code)
                .map(str::to_owned);
            let ability_name = row
                .get("GAName")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty() && *value != "None")
                .map(str::to_owned);
            let attack_type =
                classify_attack_type(category.as_deref(), effect_name, ability_name.as_deref());
            (
                effect_name.clone(),
                GameplayEffectSkill {
                    damage_source_category: category,
                    ability_name,
                    attack_type,
                },
            )
        })
        .collect())
}

pub fn load_wooden_damage_names(path: &Path) -> Result<HashMap<String, String>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("无法读取木桩伤害描述表 {}", path.display()))?;
    let document: serde_json::Value =
        serde_json::from_str(&text).context("木桩伤害描述表 JSON 无效")?;
    let rows = document
        .as_array()
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("Rows"))
        .and_then(serde_json::Value::as_object)
        .context("木桩伤害描述表缺少 Rows")?;

    Ok(rows
        .iter()
        .filter_map(|(effect_name, row)| {
            row.get("Desc")
                .and_then(|desc| desc.get("CultureInvariantString"))
                .and_then(serde_json::Value::as_str)
                .filter(|description| !description.trim().is_empty())
                .map(|description| (effect_name.clone(), normalize_damage_name(description)))
        })
        .collect())
}

pub fn normalize_damage_name(description: &str) -> String {
    description
        .replace("QTE", "环合")
        .chars()
        .filter(|character| !character.is_ascii_digit() && *character != '_')
        .collect::<String>()
        .trim()
        .to_owned()
}

fn damage_source_category_code(value: &str) -> Option<&str> {
    value
        .strip_prefix("EExecutionDamageSourceCategory::DAMAGE_SOURCE_CATEGORY_")
        .or_else(|| value.rsplit('_').next())
        .filter(|value| !value.is_empty() && *value != "NULL")
}

pub fn classify_attack_type(
    category: Option<&str>,
    effect_name: &str,
    ability_name: Option<&str>,
) -> String {
    let searchable = format!("{} {}", effect_name, ability_name.unwrap_or_default());
    let searchable_lower = searchable.to_ascii_lowercase();
    if searchable_lower.contains("tenacity") {
        return "倾陷伤害".to_owned();
    }
    if searchable_lower.contains("parry") {
        return "格挡反击".to_owned();
    }
    if effect_name.contains("Reaction_1") || effect_name.contains("Reaction1_") {
        return "创生花".to_owned();
    }
    if effect_name.contains("Reaction_2") || effect_name.contains("Reaction2_") {
        return "覆纹".to_owned();
    }
    if effect_name.contains("Reaction_3") || effect_name.contains("Reaction3_") {
        return "延滞".to_owned();
    }
    if effect_name.contains("Reaction_4") || effect_name.contains("Reaction4_") {
        return "黯星".to_owned();
    }
    if effect_name.contains("Reaction_5") || effect_name.contains("Reaction5_") {
        return "浊燃".to_owned();
    }

    let category_type = match category {
        Some("A") => Some("普攻"),
        Some("E") => Some("E技能"),
        Some("Q") => Some("Q技能"),
        Some("H") => Some("环合"),
        Some("R") => Some("反应伤害"),
        Some("Z") => Some("闪避反击"),
        _ => None,
    };
    if let Some(attack_type) = category_type {
        return attack_type.to_owned();
    }

    if searchable.contains("UltraSkill") {
        "Q技能".to_owned()
    } else if searchable.contains("QTE") || searchable.contains("EntryAttack") {
        "环合".to_owned()
    } else if searchable.contains("Melee") || searchable.contains("NormalAttack") {
        "普攻".to_owned()
    } else if searchable.contains("Skill") {
        "E技能".to_owned()
    } else {
        "其他".to_owned()
    }
}

pub fn classify_attack_type_from_description(description: &str) -> Option<String> {
    if description.contains("QTE") || description.contains("环合") {
        Some("环合".to_owned())
    } else if description.contains("大招") {
        Some("Q技能".to_owned())
    } else if description.contains("普攻") {
        Some("普攻".to_owned())
    } else if description.contains("技能") {
        Some("E技能".to_owned())
    } else {
        None
    }
}

pub fn qte_reaction_type(
    previous_attribute: &str,
    entering_attribute: &str,
    team_attributes: &HashSet<String>,
) -> Option<&'static str> {
    let has_pair = |left: &str, right: &str| {
        (previous_attribute == left && entering_attribute == right)
            || (previous_attribute == right && entering_attribute == left)
    };

    if team_attributes.contains("光")
        && team_attributes.contains("灵")
        && team_attributes.contains("相")
        && (has_pair("光", "灵") || has_pair("光", "相"))
    {
        return Some("盈蓄");
    }
    if team_attributes.contains("暗")
        && team_attributes.contains("魂")
        && team_attributes.contains("咒")
        && (has_pair("暗", "魂") || has_pair("咒", "暗"))
    {
        return Some("失谐");
    }

    if has_pair("光", "灵") {
        Some("创生")
    } else if has_pair("灵", "咒") {
        Some("覆纹")
    } else if has_pair("咒", "暗") {
        Some("浊燃")
    } else if has_pair("暗", "魂") {
        Some("黯星")
    } else if has_pair("光", "相") {
        Some("延滞")
    } else {
        None
    }
}

fn decode_shifted_into(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
    start_bit_offset: usize,
    output: &mut [u8],
) -> Option<()> {
    for (index, byte) in output.iter_mut().enumerate() {
        let bit_position = bit_shift as usize + start_bit_offset + index * 8;
        let source_offset = byte_offset + bit_position / 8;
        let source_shift = bit_position % 8;
        let current = *data.get(source_offset)?;
        let mut value = (current as u16) >> source_shift;
        if source_shift != 0 {
            value |= (*data.get(source_offset + 1)? as u16) << (8 - source_shift);
        }
        *byte = value as u8;
    }
    Some(())
}

fn decode_shifted_bytes(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
    start_bit_offset: usize,
    count: usize,
) -> Option<Vec<u8>> {
    let mut output = vec![0; count];
    decode_shifted_into(data, byte_offset, bit_shift, start_bit_offset, &mut output)?;
    Some(output)
}

fn read_field(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
    bit_offset: usize,
) -> Option<(u8, Field, usize)> {
    let mut header = [0; 5];
    decode_shifted_into(data, byte_offset, bit_shift, bit_offset, &mut header)?;
    let field_length = u32::from_le_bytes(header[1..5].try_into().ok()?) as usize;
    let consumed_bits = 40 + field_length * 8;
    let remaining_bits = data.len().saturating_sub(byte_offset) * 8;
    if field_length == 0
        || field_length > MAX_RECORD_FIELD_LENGTH
        || bit_offset + consumed_bits > remaining_bits
    {
        return None;
    }
    let mut field = Field {
        len: field_length,
        ..Default::default()
    };
    decode_shifted_into(
        data,
        byte_offset,
        bit_shift,
        bit_offset + 40,
        &mut field.raw[..field_length],
    )?;
    Some((header[0], field, consumed_bits))
}

fn f32_field(field: &Field) -> Option<f32> {
    Some(f32::from_le_bytes(field.raw[..field.len].try_into().ok()?))
}

fn f64_field(field: &Field) -> Option<f64> {
    Some(f64::from_le_bytes(field.raw[..field.len].try_into().ok()?))
}

fn i32_field(field: &Field) -> Option<i32> {
    Some(i32::from_le_bytes(field.raw[..field.len].try_into().ok()?))
}

fn parse_damage_record_at(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
) -> Option<ParsedDamageRecord> {
    let mut fields = [Field::default(); RECORD_FIELD_TYPES.len()];
    let mut bit_cursor = 0;
    for (index, (expected_type, expected_length)) in RECORD_FIELD_TYPES
        .into_iter()
        .zip(RECORD_FIELD_LENGTHS)
        .enumerate()
    {
        let (field_type, field, consumed) = read_field(data, byte_offset, bit_shift, bit_cursor)?;
        if field_type != expected_type || field.len != expected_length {
            return None;
        }
        bit_cursor += consumed;
        fields[index] = field;
    }

    let damage = f32_field(&fields[0])?;
    let target_hp_before = f32_field(&fields[1])?;
    let target_max_hp = f32_field(&fields[2])?;
    let damage_time = f64_field(&fields[3])?;
    let world_time = f32_field(&fields[4])?;
    let repeated_damage = f32_field(&fields[5])?;
    let state_flags = [
        i32_field(&fields[6])?,
        i32_field(&fields[7])?,
        i32_field(&fields[8])?,
    ];
    let trailing_value = f32_field(&fields[9])?;

    if !damage.is_finite()
        || !(MIN_DAMAGE..=MAX_DAMAGE).contains(&damage)
        || !target_hp_before.is_finite()
        || target_hp_before < 0.0
        || !target_max_hp.is_finite()
        || target_max_hp <= 0.0
        || !damage_time.is_finite()
        || damage_time < 0.0
        || !world_time.is_finite()
        || world_time < 0.0
        || !trailing_value.is_finite()
    {
        return None;
    }

    let tolerance = 0.01_f32.max(damage.abs() * 1e-6);
    if (damage - repeated_damage).abs() > tolerance {
        return None;
    }

    Some(ParsedDamageRecord {
        damage,
        target_hp_before,
        target_max_hp,
        damage_time,
        world_time,
        repeated_damage,
        state_flags,
        trailing_value,
        byte_offset: byte_offset + 5,
        bit_shift,
    })
}

pub fn parse_damage_records(data: &[u8]) -> Vec<ParsedDamageRecord> {
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for byte_offset in 0..data.len() {
        for bit_shift in 0..8_u8 {
            let Some(record) = parse_damage_record_at(data, byte_offset, bit_shift) else {
                continue;
            };
            if seen.insert((byte_offset, bit_shift)) {
                records.push(record);
            }
        }
    }
    records
}

pub fn parse_current_hp_updates(data: &[u8]) -> Vec<ParsedCurrentHpUpdate> {
    let mut updates = Vec::new();
    for byte_offset in 0..data.len() {
        for bit_shift in 0..8_u8 {
            let mut decoded = [0; CURRENT_HP_PREFIX_LENGTH + 4];
            if decode_shifted_into(data, byte_offset, bit_shift, 0, &mut decoded).is_none() {
                continue;
            }
            let prefix = &decoded[..CURRENT_HP_PREFIX_LENGTH];
            if prefix[1..7] != [0, 0, 0xe0, 0x4f, 0x33, 0x33]
                || prefix[8] != 0x0f
                || prefix[11..16] != [0, 0, 0, 0, 0x24]
            {
                continue;
            }
            let current_hp =
                f32::from_le_bytes([decoded[16], decoded[17], decoded[18], decoded[19]]);
            if !current_hp.is_finite() || !(0.0..=MAX_PLAUSIBLE_CHARACTER_HP).contains(&current_hp)
            {
                continue;
            }
            updates.push(ParsedCurrentHpUpdate {
                current_hp,
                byte_offset: byte_offset + CURRENT_HP_PREFIX_LENGTH,
                bit_shift,
            });
        }
    }
    updates
}

pub fn parse_boss_hp_updates(data: &[u8]) -> Vec<ParsedBossHpUpdate> {
    let mut updates = Vec::new();
    for byte_offset in 0..data.len() {
        for bit_shift in 0..8_u8 {
            let mut decoded = [0; BOSS_HP_PREFIX_LENGTH + 4];
            if decode_shifted_into(data, byte_offset, bit_shift, 0, &mut decoded).is_none()
                || decoded[..BOSS_HP_PREFIX_HEAD.len()] != BOSS_HP_PREFIX_HEAD
                || decoded[8..24].iter().all(|byte| *byte == 0)
                || decoded[24..BOSS_HP_PREFIX_LENGTH]
                    .iter()
                    .any(|byte| *byte != 0)
            {
                continue;
            }
            let current_hp = f32::from_le_bytes(
                decoded[BOSS_HP_PREFIX_LENGTH..]
                    .try_into()
                    .expect("Boss HP field has a fixed four-byte length"),
            );
            if !current_hp.is_finite() || !(0.0..=MAX_DAMAGE).contains(&current_hp) {
                continue;
            }
            updates.push(ParsedBossHpUpdate {
                target_handle: decoded[8..24]
                    .try_into()
                    .expect("Boss target handle has a fixed 16-byte length"),
                current_hp,
                byte_offset: byte_offset + BOSS_HP_PREFIX_LENGTH,
                bit_shift,
            });
        }
    }
    updates
}

pub fn parse_object_handles(data: &[u8]) -> Vec<[u8; 16]> {
    let mut handles = Vec::new();
    let mut seen = HashSet::new();
    for bit_shift in 0..8_u8 {
        let shifted = if bit_shift == 0 {
            data.to_vec()
        } else {
            match decode_shifted_bytes(data, 0, bit_shift, 0, data.len().saturating_sub(1)) {
                Some(value) => value,
                None => continue,
            }
        };
        for window in shifted.windows(28) {
            let handle: [u8; 16] = window[..16]
                .try_into()
                .expect("Object handle window has a fixed 16-byte length");
            if handle.iter().all(|byte| *byte != 0)
                && window[16..].iter().all(|byte| *byte == 0)
                && seen.insert(handle)
            {
                handles.push(handle);
            }
        }
    }
    handles
}

pub fn parse_gameplay_effects(data: &[u8]) -> Vec<ParsedGameplayEffect> {
    let mut effects = Vec::new();
    let mut seen = HashSet::new();
    for bit_shift in 0..8_u8 {
        let shifted = if bit_shift == 0 {
            data.to_vec()
        } else {
            match decode_shifted_bytes(data, 0, bit_shift, 0, data.len().saturating_sub(1)) {
                Some(value) => value,
                None => continue,
            }
        };
        for (anchor_offset, window) in shifted
            .windows(ACTIVE_GAMEPLAY_EFFECT_ANCHOR.len())
            .enumerate()
        {
            if window != ACTIVE_GAMEPLAY_EFFECT_ANCHOR {
                continue;
            }
            let marker_offset = anchor_offset
                + ACTIVE_GAMEPLAY_EFFECT_ANCHOR.len()
                + ACTIVE_GAMEPLAY_EFFECT_VALUE_OFFSET;
            let Some(marker_bytes) = shifted.get(marker_offset..marker_offset + 4) else {
                continue;
            };
            let marker = u32::from_le_bytes(marker_bytes.try_into().unwrap());
            if marker != ACTIVE_GAMEPLAY_EFFECT_MARKER {
                continue;
            }
            let index_offset = marker_offset + 4;
            let Some(index_bytes) = shifted.get(index_offset..index_offset + 4) else {
                continue;
            };
            let unique_index = u32::from_le_bytes(index_bytes.try_into().unwrap());
            if matches!(unique_index, 0 | u32::MAX)
                || !seen.insert((unique_index, bit_shift, index_offset))
            {
                continue;
            }
            effects.push(ParsedGameplayEffect {
                unique_index,
                byte_offset: index_offset,
                bit_shift,
            });
        }
    }
    effects
}

pub fn find_declared_character_evidence(data: &[u8]) -> Vec<(u32, u8, usize)> {
    let mut found = Vec::new();
    for bit_shift in 0..8 {
        let shifted = if bit_shift == 0 {
            data.to_vec()
        } else {
            match decode_shifted_bytes(data, 0, bit_shift, 0, data.len().saturating_sub(1)) {
                Some(value) => value,
                None => continue,
            }
        };
        if shifted.len() < 9 {
            continue;
        }
        for offset in 0..=shifted.len() - 9 {
            let row = &shifted[offset..offset + 9];
            if row[..4] != [5, 0, 0, 0] || row[8] != 0 {
                continue;
            }
            if row[4..8].iter().all(u8::is_ascii_digit) {
                let id = row[4..8]
                    .iter()
                    .fold(0_u32, |value, digit| value * 10 + (digit - b'0') as u32);
                let evidence = (id, bit_shift, offset);
                if (1000..=9999).contains(&id) && !found.contains(&evidence) {
                    found.push(evidence);
                }
            }
        }
    }
    found
}

pub fn declared_character_ids_from_evidence(evidence: &[(u32, u8, usize)]) -> Vec<u32> {
    let mut ids = Vec::new();
    for (id, _, _) in evidence {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    ids
}

fn declared_character_for_shift(evidence: &[(u32, u8, usize)], bit_shift: u8) -> Option<u32> {
    let mut matched = None;
    for (id, shift, _) in evidence {
        if *shift != bit_shift {
            continue;
        }
        if matched.is_some_and(|current| current != *id) {
            return None;
        }
        matched = Some(*id);
    }
    matched
}

pub fn parse_damage_payload(
    data: &[u8],
    timestamp: f64,
    packet_char_id: Option<u32>,
    fallback_char_id: Option<u32>,
    characters: &HashMap<u32, CharacterInfo>,
    evidence: &[(u32, u8, usize)],
) -> Vec<Hit> {
    let mut hits = Vec::new();
    for record in parse_damage_records(data) {
        let damage = record.damage;
        let byte_offset = record.byte_offset;
        let bit_shift = record.bit_shift;
        let aligned_char_id = declared_character_for_shift(evidence, bit_shift);
        let resolved_packet_char_id = packet_char_id.or(aligned_char_id);
        let char_id = resolved_packet_char_id.or(fallback_char_id).unwrap_or(0);
        let target_hp_before = record.target_hp_before;
        let target_max_hp = record.target_max_hp;
        let character = characters.get(&char_id);
        let name = character
            .map(|row| {
                if row.name_zh.is_empty() {
                    row.name_en.clone()
                } else {
                    row.name_zh.clone()
                }
            })
            .unwrap_or_else(|| {
                if char_id == 0 {
                    "未知角色".to_owned()
                } else {
                    format!("未知角色({char_id})")
                }
            });
        let target_hp_after = (target_hp_before - damage).max(0.0);
        let (mut target_id, mut target_name, mut target_context) =
            extract_target_metadata(data, byte_offset, bit_shift);
        let direction = if target_max_hp <= MAX_PLAUSIBLE_CHARACTER_HP
            && resolved_packet_char_id.is_some()
            && evidence
                .iter()
                .any(|(id, shift, _)| Some(*id) == resolved_packet_char_id && *shift == bit_shift)
        {
            "incoming"
        } else if resolved_packet_char_id.is_some() {
            "outgoing"
        } else {
            "unknown"
        };
        if direction == "incoming" {
            target_id = Some(char_id.to_string());
            target_name = Some(name.clone());
            target_context = vec!["受击目标：封包内同位角色声明".to_owned()];
        }

        hits.push(Hit {
            timestamp,
            char_id,
            char_name: name,
            char_known: character.is_some(),
            damage: damage as f64,
            byte_offset,
            bit_shift,
            char_source: if resolved_packet_char_id.is_some() {
                "packet"
            } else if fallback_char_id.is_some() {
                "session"
            } else {
                "unknown"
            }
            .to_owned(),
            direction: direction.to_owned(),
            target_hp_before: target_hp_before as f64,
            target_hp_after: target_hp_after as f64,
            target_max_hp: target_max_hp as f64,
            target_hp_percent: if target_max_hp > 0.0 {
                target_hp_after as f64 / target_max_hp as f64 * 100.0
            } else {
                0.0
            },
            target_id,
            target_name,
            target_context,
            gameplay_effect_index: None,
            gameplay_effect_name: None,
            ability_name: None,
            damage_name: None,
            attack_type: None,
        });
    }
    hits
}

fn extract_target_metadata(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
) -> (Option<String>, Option<String>, Vec<String>) {
    let start = byte_offset.saturating_sub(1_024);
    let end = data.len().min(byte_offset.saturating_add(1_024));
    let mut shifted = vec![0; end - start];
    if decode_shifted_into(data, start, bit_shift, 0, &mut shifted).is_none() {
        return (None, None, Vec::new());
    }
    let mut all_strings = Vec::new();
    let mut strings = Vec::new();
    let mut cursor = 0;
    while cursor < shifted.len() {
        if shifted[cursor].is_ascii_graphic() || shifted[cursor] == b' ' {
            let begin = cursor;
            while cursor < shifted.len()
                && (shifted[cursor].is_ascii_graphic() || shifted[cursor] == b' ')
            {
                cursor += 1;
            }
            if cursor - begin >= 4 {
                let value = String::from_utf8_lossy(&shifted[begin..cursor])
                    .trim()
                    .to_owned();
                if !all_strings.contains(&value) {
                    all_strings.push(value.clone());
                }
                let lower = value.to_ascii_lowercase();
                if [
                    "target",
                    "boss",
                    "enemy",
                    "monster",
                    "actor",
                    "entity",
                    "npc",
                    "characterfornet",
                ]
                .iter()
                .any(|marker| lower.contains(marker))
                    && !strings.contains(&value)
                {
                    strings.push(value);
                }
            }
        } else {
            cursor += 1;
        }
    }

    let target_id = strings
        .iter()
        .find_map(|value| metadata_value(value, &["targetid", "target_id", "entityid", "actorid"]));
    let mut target_name = strings.iter().find_map(|value| {
        metadata_value(
            value,
            &[
                "targetname",
                "target_name",
                "enemyname",
                "monstername",
                "bossname",
            ],
        )
    });
    let mut resolved_monsters = all_strings
        .iter()
        .flat_map(|value| {
            value
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .filter(|token| !token.is_empty())
        })
        .filter_map(monster_entry)
        .collect::<Vec<_>>();
    resolved_monsters.sort_by(|left, right| left.id.cmp(&right.id));
    resolved_monsters.dedup_by(|left, right| left.id == right.id);
    let mut target_id = target_id;
    if resolved_monsters.len() == 1 {
        let monster = resolved_monsters.remove(0);
        target_id.get_or_insert(monster.id);
        target_name.get_or_insert(monster.display_name);
    }
    (target_id, target_name, strings)
}

fn monster_entry(value: &str) -> Option<MonsterIndexEntry> {
    let normalized = value
        .strip_suffix("_C")
        .or_else(|| value.strip_suffix("_c"))
        .unwrap_or(value)
        .to_ascii_lowercase();
    monster_index().get(&normalized).cloned()
}

pub fn detect_monster_targets(data: &[u8]) -> Vec<(String, String)> {
    let mut monsters = Vec::new();
    for bit_shift in 0..8_u8 {
        let shifted = if bit_shift == 0 {
            data.to_vec()
        } else {
            match decode_shifted_bytes(data, 0, bit_shift, 0, data.len().saturating_sub(1)) {
                Some(value) => value,
                None => continue,
            }
        };
        let text = String::from_utf8_lossy(&shifted);
        for token in
            text.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        {
            let Some((id, display_name)) = monster_entry(token)
                .map(|monster| (monster.id, monster.display_name))
                .or_else(|| monster_target_from_identifier(token))
            else {
                continue;
            };
            let entry = (id, display_name);
            if !monsters.contains(&entry) {
                monsters.push(entry);
            }
        }
    }
    monsters
}

pub fn monster_target_from_identifier(value: &str) -> Option<(String, String)> {
    if let Some(monster) = monster_entry(value) {
        return Some((monster.id, monster.display_name));
    }
    let lower = value.to_ascii_lowercase();
    for marker in ["boss_", "mon_"] {
        let Some(start) = lower.find(marker) else {
            continue;
        };
        let raw_digits = lower[start + marker.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        if raw_digits.is_empty() {
            continue;
        }
        let normalized_digits = raw_digits.trim_start_matches('0');
        for family in [
            format!("{marker}{raw_digits}"),
            format!("{marker}{raw_digits}_bp"),
            format!("{marker}{normalized_digits}"),
            format!("{marker}{normalized_digits}_bp"),
        ] {
            if let Some(monster) = monster_index().get(&family) {
                return Some((monster.id.clone(), monster.display_name.clone()));
            }
        }
    }
    None
}

fn metadata_value(value: &str, keys: &[&str]) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    for key in keys {
        let Some(index) = lower.find(key) else {
            continue;
        };
        let tail = value[index + key.len()..]
            .trim_start_matches(|character: char| {
                character.is_ascii_whitespace() || matches!(character, ':' | '=' | '#')
            })
            .split(|character: char| {
                character.is_ascii_whitespace() || matches!(character, ',' | ';' | '|' | '\0')
            })
            .next()
            .unwrap_or_default()
            .trim();
        if !tail.is_empty() && tail.len() <= 96 {
            return Some(tail.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod character_tests {
    use super::*;

    fn encoded_damage_record(damage: f32, target_hp_before: f32, target_max_hp: f32) -> Vec<u8> {
        let fields = [
            (12, damage.to_le_bytes().to_vec()),
            (12, target_hp_before.to_le_bytes().to_vec()),
            (12, target_max_hp.to_le_bytes().to_vec()),
            (13, 1.0_f64.to_le_bytes().to_vec()),
            (12, 1.0_f32.to_le_bytes().to_vec()),
            (12, damage.to_le_bytes().to_vec()),
            (6, 0_i32.to_le_bytes().to_vec()),
            (6, 0_i32.to_le_bytes().to_vec()),
            (6, 0_i32.to_le_bytes().to_vec()),
            (12, 0.0_f32.to_le_bytes().to_vec()),
        ];
        let mut encoded = Vec::new();
        for (field_type, value) in fields {
            encoded.push(field_type);
            encoded.extend_from_slice(&(value.len() as u32).to_le_bytes());
            encoded.extend_from_slice(&value);
        }
        encoded
    }

    #[test]
    fn character_attribute_is_optional_and_loaded_when_present() {
        let document: CharacterDocument = serde_json::from_str(
            r#"{
                "characters": {
                    "1003": {"name_zh": "Sagiri"},
                    "1010": {"name_zh": "Nanally", "attribute": "curse"}
                }
            }"#,
        )
        .unwrap();

        assert_eq!(document.characters["1003"].attribute, None);
        assert_eq!(
            document.characters["1010"].attribute.as_deref(),
            Some("curse")
        );
    }

    #[test]
    fn parses_gameplay_effect_unique_index_after_active_ge_anchor() {
        let mut payload = vec![0xaa, 0xbb];
        payload.extend_from_slice(ACTIVE_GAMEPLAY_EFFECT_ANCHOR);
        payload.extend_from_slice(&[0; ACTIVE_GAMEPLAY_EFFECT_VALUE_OFFSET]);
        payload.extend_from_slice(&ACTIVE_GAMEPLAY_EFFECT_MARKER.to_le_bytes());
        payload.extend_from_slice(&1012_u32.to_le_bytes());
        payload.extend_from_slice(&[5, 0, 0, 0]);

        assert_eq!(
            parse_gameplay_effects(&payload),
            vec![ParsedGameplayEffect {
                unique_index: 1012,
                byte_offset: 2
                    + ACTIVE_GAMEPLAY_EFFECT_ANCHOR.len()
                    + ACTIVE_GAMEPLAY_EFFECT_VALUE_OFFSET
                    + 4,
                bit_shift: 0,
            }]
        );
    }

    #[test]
    fn loads_gameplay_effect_names_from_assets() {
        let mapping =
            load_gameplay_effect_mapping(Path::new(GAMEPLAY_EFFECT_MAPPING_PATH)).unwrap();

        assert_eq!(
            mapping.get(&1012).map(String::as_str),
            Some("GE_Player_Sagiri_QTE1_Damage")
        );
    }

    #[test]
    fn loads_attack_types_from_skill_damage_assets() {
        let skills = load_gameplay_effect_skills(Path::new(SKILL_DAMAGE_DATA_PATH)).unwrap();

        for (effect, expected_type, expected_ability) in [
            (
                "GE_Player_Nanally_Melee1_Damage",
                "普攻",
                "GA_Nanally_Melee",
            ),
            (
                "GE_Player_Nanally_Skill1_Damage",
                "E技能",
                "GA_Nanally_Skill",
            ),
            (
                "GE_Player_Nanally_UltraSkill1_Damage",
                "Q技能",
                "GA_Nanally_UltraSkill",
            ),
            ("GE_Player_Sagiri_QTE1_Damage", "环合", "GA_Sagiri_QTE"),
            (
                "GE_Player_Nanally_PerfectEvadeAttack_Damage",
                "闪避反击",
                "GA_Nanally_ExtremEvadeAtk",
            ),
        ] {
            let skill = skills.get(effect).unwrap();
            assert_eq!(skill.attack_type, expected_type);
            assert_eq!(skill.ability_name.as_deref(), Some(expected_ability));
        }
    }

    #[test]
    fn loads_chinese_damage_names_from_wooden_assets() {
        let names = load_wooden_damage_names(Path::new(WOODEN_DAMAGE_DESCRIPTIONS_PATH)).unwrap();

        assert_eq!(
            names
                .get("GE_Player_Sagiri_QTE1_Damage")
                .map(String::as_str),
            Some("早雾环合")
        );
        assert_eq!(
            names
                .get("GE_Player_Nanally_Melee1_Damage")
                .map(String::as_str),
            Some("娜娜莉普攻")
        );
        assert_eq!(
            classify_attack_type_from_description("早雾大招1").as_deref(),
            Some("Q技能")
        );
    }

    #[test]
    fn normalizes_damage_names_for_grouping() {
        assert_eq!(normalize_damage_name("早雾大招2"), "早雾大招");
        assert_eq!(normalize_damage_name("早雾普攻2_1"), "早雾普攻");
        assert_eq!(normalize_damage_name("哈索尔QTE2"), "哈索尔环合");
        assert_eq!(normalize_damage_name("法帝娅大招追加3_2"), "法帝娅大招追加");
    }

    #[test]
    fn classifies_tenacity_and_parry_damage() {
        assert_eq!(
            classify_attack_type(Some("B"), "Buff_Tenacity_damage", None,),
            "倾陷伤害"
        );
        assert_eq!(
            classify_attack_type(Some("T"), "GE_Parry_Damage", None,),
            "格挡反击"
        );
    }

    #[test]
    fn classifies_qte_reaction_from_participant_and_team_attributes() {
        let attributes = ["灵", "咒", "暗"].into_iter().map(str::to_owned).collect();
        assert_eq!(qte_reaction_type("暗", "咒", &attributes), Some("浊燃"));
        assert_eq!(qte_reaction_type("灵", "咒", &attributes), Some("覆纹"));

        let accumulation = ["光", "灵", "相"].into_iter().map(str::to_owned).collect();
        assert_eq!(qte_reaction_type("光", "灵", &accumulation), Some("盈蓄"));

        let discord = ["暗", "魂", "咒"].into_iter().map(str::to_owned).collect();
        assert_eq!(qte_reaction_type("暗", "咒", &discord), Some("失谐"));
    }

    #[test]
    fn ignores_invalid_gameplay_effect_sentinel() {
        let mut payload = ACTIVE_GAMEPLAY_EFFECT_ANCHOR.to_vec();
        payload.extend_from_slice(&[0; ACTIVE_GAMEPLAY_EFFECT_VALUE_OFFSET]);
        payload.extend_from_slice(&ACTIVE_GAMEPLAY_EFFECT_MARKER.to_le_bytes());
        payload.extend_from_slice(&u32::MAX.to_le_bytes());

        assert!(parse_gameplay_effects(&payload).is_empty());
    }

    #[test]
    fn parses_target_handle_from_boss_hp_update() {
        let handle = [
            0x21, 0xf0, 0x4e, 0x92, 0x89, 0x95, 0x33, 0x4f, 0x8c, 0x0b, 0xbc, 0xaa, 0x0e, 0xe1,
            0x6f, 0xe7,
        ];
        let mut payload = [0_u8; BOSS_HP_PREFIX_LENGTH + 4];
        payload[..8].copy_from_slice(&BOSS_HP_PREFIX_HEAD);
        payload[8..24].copy_from_slice(&handle);
        payload[BOSS_HP_PREFIX_LENGTH..].copy_from_slice(&1_927_891_f32.to_le_bytes());

        let updates = parse_boss_hp_updates(&payload);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].target_handle, handle);
        assert_eq!(updates[0].current_hp, 1_927_891.0);
        assert!(parse_object_handles(&payload).contains(&handle));
    }

    #[test]
    fn incoming_damage_targets_declared_character() {
        let mut payload = b"/Game/Blueprints/Character/Monster/boss_07/".to_vec();
        payload.extend_from_slice(&encoded_damage_record(100.0, 10_000.0, 20_000.0));
        payload.extend_from_slice(&[5, 0, 0, 0, b'1', b'0', b'0', b'1', 0]);
        let evidence = find_declared_character_evidence(&payload);
        let characters = HashMap::from([(
            1001,
            CharacterInfo {
                name_zh: "娜娜莉".to_owned(),
                name_en: String::new(),
                color: None,
                avatar: None,
                attribute: None,
            },
        )]);

        let hits = parse_damage_payload(&payload, 1.0, Some(1001), None, &characters, &evidence);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].direction, "incoming");
        assert_eq!(hits[0].target_id.as_deref(), Some("1001"));
        assert_eq!(hits[0].target_name.as_deref(), Some("娜娜莉"));
        assert!(
            hits[0]
                .target_context
                .iter()
                .all(|context| !context.to_ascii_lowercase().contains("boss"))
        );
    }

    #[test]
    fn resolves_character_from_matching_bit_alignment() {
        let evidence = [(1003, 2, 241), (1004, 3, 50), (1003, 0, 606)];

        assert_eq!(declared_character_for_shift(&evidence, 3), Some(1004));
        assert_eq!(declared_character_for_shift(&evidence, 2), Some(1003));
        assert_eq!(declared_character_for_shift(&evidence, 5), None);
    }

    #[test]
    fn resolves_monster_class_near_damage_record() {
        let payload = b"/Game/Blueprints/Character/Monster/Boss_017_BP_Abyss.Boss_017_BP_Abyss_C";
        let (target_id, target_name, _) = extract_target_metadata(payload, payload.len() / 2, 0);

        assert_eq!(target_id.as_deref(), Some("Boss_017_BP_Abyss"));
        assert_eq!(target_name.as_deref(), Some("玛门"));
        assert_eq!(
            detect_monster_targets(payload),
            vec![("Boss_017_BP_Abyss".to_owned(), "玛门".to_owned())]
        );
        assert_eq!(
            monster_target_from_identifier("GE_Boss_017_act01_Dmg_05a_Steal_BP"),
            Some(("Boss_017_BP_Abyss".to_owned(), "玛门".to_owned()))
        );
        assert_eq!(
            monster_target_from_identifier("boss_07"),
            Some(("Boss_07_BP".to_owned(), "塞润尼缇".to_owned()))
        );
    }

    #[test]
    fn resolves_monster_icons_for_family_variants() {
        assert_eq!(
            monster_icon_path(Some("Boss_07_BP_Abyss"), Some("塞润尼缇")),
            Some("res/images/monsters/Boss_07.png")
        );
        assert_eq!(
            monster_icon_path(Some("Boss_017_BP_Abyss"), Some("玛门")),
            Some("res/images/monsters/Boss_17.png")
        );
        assert_eq!(
            monster_icon_path(Some("mon_18_2_BP_Abyss"), Some("棉绒绒(变身)")),
            Some("res/images/monsters/mon_18_2.png")
        );
        assert_eq!(
            monster_icon_path(Some("mon_35_BP_Red_Abyss"), None),
            Some("res/images/monsters/mon_35_red.png")
        );
    }
}
