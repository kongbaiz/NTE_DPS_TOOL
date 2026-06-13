use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

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
    pub current_hp: f32,
    pub byte_offset: usize,
    pub bit_shift: u8,
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
                current_hp,
                byte_offset: byte_offset + BOSS_HP_PREFIX_LENGTH,
                bit_shift,
            });
        }
    }
    updates
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
        let char_id = packet_char_id.or(fallback_char_id).unwrap_or(0);
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
        let (target_id, target_name, target_context) =
            extract_target_metadata(data, byte_offset, bit_shift);
        let direction = if target_max_hp <= MAX_PLAUSIBLE_CHARACTER_HP
            && packet_char_id.is_some()
            && evidence
                .iter()
                .any(|(id, shift, _)| Some(*id) == packet_char_id && *shift == bit_shift)
        {
            "incoming"
        } else if packet_char_id.is_some() {
            "outgoing"
        } else {
            "unknown"
        };

        hits.push(Hit {
            timestamp,
            char_id,
            char_name: name,
            char_known: character.is_some(),
            damage: damage as f64,
            byte_offset,
            bit_shift,
            char_source: if packet_char_id.is_some() {
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
        });
    }
    hits
}

fn extract_target_metadata(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
) -> (Option<String>, Option<String>, Vec<String>) {
    let start = byte_offset.saturating_sub(256);
    let end = data.len().min(byte_offset.saturating_add(320));
    let mut shifted = vec![0; end - start];
    if decode_shifted_into(data, start, bit_shift, 0, &mut shifted).is_none() {
        return (None, None, Vec::new());
    }
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
    let target_name = strings.iter().find_map(|value| {
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
    (target_id, target_name, strings)
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
}
