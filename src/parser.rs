use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::{CharacterInfo, Hit};

const FIELD_SEPARATORS: [u8; 9] = [12, 12, 13, 12, 12, 6, 6, 6, 12];
const SCAN_START: usize = 200;
const SCAN_END: usize = 700;
const MAX_FIELD_LENGTH: usize = 64;
const MIN_DAMAGE: f32 = 2.0;
const MAX_DAMAGE: f32 = 1_000_000_000.0;
const MAX_PLAUSIBLE_CHARACTER_HP: f32 = 500_000.0;

#[derive(Deserialize)]
struct CharacterDocument {
    characters: HashMap<String, CharacterInfo>,
}

#[derive(Clone, Debug)]
struct Field {
    separator: u8,
    raw: Vec<u8>,
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

fn decode_shifted_bytes(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
    start_bit_offset: usize,
    count: usize,
) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(count);
    for index in 0..count {
        let bit_position = bit_shift as usize + start_bit_offset + index * 8;
        let source_offset = byte_offset + bit_position / 8;
        let source_shift = bit_position % 8;
        let current = *data.get(source_offset)?;
        let mut value = (current as u16) >> source_shift;
        if source_shift != 0 {
            value |= (*data.get(source_offset + 1)? as u16) << (8 - source_shift);
        }
        output.push(value as u8);
    }
    Some(output)
}

fn read_field(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
    bit_offset: usize,
) -> Option<(Field, usize)> {
    let header = decode_shifted_bytes(data, byte_offset, bit_shift, bit_offset, 5)?;
    let field_length = u32::from_le_bytes(header[1..5].try_into().ok()?) as usize;
    let consumed_bits = 40 + field_length * 8;
    let remaining_bits = data.len().saturating_sub(byte_offset) * 8;
    if field_length == 0
        || field_length > MAX_FIELD_LENGTH
        || bit_offset + consumed_bits > remaining_bits
    {
        return None;
    }
    let raw = decode_shifted_bytes(data, byte_offset, bit_shift, bit_offset + 40, field_length)?;
    Some((
        Field {
            separator: header[0],
            raw,
        },
        consumed_bits,
    ))
}

fn f32_field(field: &Field) -> Option<f32> {
    Some(f32::from_le_bytes(field.raw.get(..4)?.try_into().ok()?))
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

pub fn declared_character_ids(data: &[u8]) -> Vec<u32> {
    let mut ids = Vec::new();
    for (id, _, _) in find_declared_character_evidence(data) {
        if !ids.contains(&id) {
            ids.push(id);
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
    let mut seen = HashSet::new();
    for byte_offset in SCAN_START..data.len().min(SCAN_END) {
        for bit_shift in 0..8_u8 {
            let Some(raw_damage) = decode_shifted_bytes(data, byte_offset, bit_shift, 0, 4) else {
                continue;
            };
            let damage = f32::from_le_bytes(raw_damage.try_into().unwrap());
            if !damage.is_finite() || !(MIN_DAMAGE..=MAX_DAMAGE).contains(&damage) {
                continue;
            }

            let mut fields = Vec::with_capacity(9);
            let mut bit_cursor = 32;
            for expected_separator in FIELD_SEPARATORS {
                let Some((field, consumed)) = read_field(data, byte_offset, bit_shift, bit_cursor)
                else {
                    break;
                };
                if field.separator != expected_separator {
                    break;
                }
                bit_cursor += consumed;
                fields.push(field);
            }
            if fields.len() != 9 {
                continue;
            }
            let Some(repeated_damage) = f32_field(&fields[4]) else {
                continue;
            };
            let tolerance = 0.01_f32.max(damage.abs() * 1e-6);
            if (damage - repeated_damage).abs() > tolerance {
                continue;
            }

            let char_id = packet_char_id.or(fallback_char_id).unwrap_or(0);
            let Some(target_hp_before) = f32_field(&fields[0]) else {
                continue;
            };
            let Some(target_max_hp) = f32_field(&fields[1]) else {
                continue;
            };
            let key = (char_id, damage.round() as i64, byte_offset, bit_shift);
            if !seen.insert(key) {
                continue;
            }
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
    let shifted: Vec<u8> = (start..end)
        .filter_map(|index| decode_shifted_bytes(data, index, bit_shift, 0, 1))
        .map(|value| value[0])
        .collect();
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
mod tests {
    use super::*;

    fn synthetic_payload(char_id: u32, damage: f32) -> Vec<u8> {
        let mut data = vec![0_u8; 512];
        let declaration = format!("\x05\0\0\0{char_id:04}\0");
        data[10..19].copy_from_slice(declaration.as_bytes());
        let mut cursor = SCAN_START;
        data[cursor..cursor + 4].copy_from_slice(&damage.to_le_bytes());
        cursor += 4;
        for (index, separator) in FIELD_SEPARATORS.iter().enumerate() {
            let raw = if index == 2 {
                12345_f64.to_le_bytes().to_vec()
            } else if index == 4 {
                damage.to_le_bytes().to_vec()
            } else {
                ((index + 1) as f32).to_le_bytes().to_vec()
            };
            data[cursor] = *separator;
            data[cursor + 1..cursor + 5].copy_from_slice(&(raw.len() as u32).to_le_bytes());
            data[cursor + 5..cursor + 5 + raw.len()].copy_from_slice(&raw);
            cursor += 5 + raw.len();
        }
        data
    }

    #[test]
    fn parses_python_compatible_payload() {
        let data = synthetic_payload(1033, 12345.5);
        let evidence = find_declared_character_evidence(&data);
        let hits = parse_damage_payload(&data, 1.0, Some(1033), None, &HashMap::new(), &evidence);
        assert_eq!(declared_character_ids(&data), vec![1033]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].damage, 12345.5);
    }

    #[test]
    fn does_not_classify_enemy_health_as_incoming_damage() {
        let mut data = synthetic_payload(1004, 3480.0);
        let max_hp = 1_528_385_f32.to_le_bytes();
        let max_hp_field_offset = SCAN_START + 4 + 5 + 4 + 5;
        data[max_hp_field_offset..max_hp_field_offset + 4].copy_from_slice(&max_hp);
        let evidence = find_declared_character_evidence(&data);
        let hits = parse_damage_payload(&data, 1.0, Some(1004), None, &HashMap::new(), &evidence);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].direction, "outgoing");
    }

    #[test]
    fn extracts_explicit_target_metadata_when_present() {
        let mut data = synthetic_payload(1033, 12345.5);
        let metadata = b"TargetId=enemy-42 TargetName=TrainingDummy";
        data[320..320 + metadata.len()].copy_from_slice(metadata);
        let evidence = find_declared_character_evidence(&data);
        let hits = parse_damage_payload(&data, 1.0, Some(1033), None, &HashMap::new(), &evidence);

        assert_eq!(hits[0].target_id.as_deref(), Some("enemy-42"));
        assert_eq!(hits[0].target_name.as_deref(), Some("TrainingDummy"));
        assert!(!hits[0].target_context.is_empty());
    }

    #[test]
    fn matches_recorded_packet_log() {
        let path = Path::new("logs/nte_raw_packets_20260611_000752.jsonl");
        if !path.exists() {
            return;
        }
        let text = fs::read_to_string(path).unwrap();
        let row: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        let payload = hex::decode(row["payload_hex"].as_str().unwrap()).unwrap();
        let timestamp = row["timestamp"].as_f64().unwrap();
        let expected = &row["parsed_hits"][0];
        let ids = declared_character_ids(&payload);
        let evidence = find_declared_character_evidence(&payload);
        let hits = parse_damage_payload(
            &payload,
            timestamp,
            ids.first().copied(),
            None,
            &HashMap::new(),
            &evidence,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].char_id,
            expected["char_id"].as_u64().unwrap() as u32
        );
        assert_eq!(hits[0].damage, expected["damage"].as_f64().unwrap());
        assert_eq!(
            hits[0].byte_offset,
            expected["byte_offset"].as_u64().unwrap() as usize
        );
        assert_eq!(
            hits[0].bit_shift,
            expected["bit_shift"].as_u64().unwrap() as u8
        );
    }
}
