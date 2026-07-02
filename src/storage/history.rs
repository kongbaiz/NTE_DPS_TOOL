use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::engine::model::{
    CombatSessionCharacterSummary, CombatSessionSkillSummary, CombatSessionSummary, TeamDps,
    TeamDpsMember,
};
use crate::storage::io_util::atomic_write_text;

pub const HISTORY_RECORD_VERSION: u32 = 1;
pub const MAX_HISTORY_RECORDS: usize = 200;

static HISTORY_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryRecord {
    pub version: u32,
    pub id: String,
    pub saved_at: DateTime<Utc>,
    pub summary: CombatSessionSummary,
}

impl Default for HistoryRecord {
    fn default() -> Self {
        Self {
            version: HISTORY_RECORD_VERSION,
            id: String::new(),
            saved_at: Utc::now(),
            summary: CombatSessionSummary::default(),
        }
    }
}

impl HistoryRecord {
    pub fn display_time(&self) -> String {
        self.saved_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    pub fn to_team_dps(&self) -> Option<TeamDps> {
        team_from_characters(self.summary.total_dps, &self.summary.characters)
    }

    pub fn upper_team_dps(&self) -> Option<TeamDps> {
        self.summary
            .abyss
            .first_half
            .as_ref()
            .and_then(|half| team_from_characters(half.total_dps, &half.characters))
            .or_else(|| self.to_team_dps())
    }

    pub fn lower_team_dps(&self) -> Option<TeamDps> {
        self.summary
            .abyss
            .second_half
            .as_ref()
            .and_then(|half| team_from_characters(half.total_dps, &half.characters))
            .or_else(|| self.to_team_dps())
    }
}

fn team_from_characters(dps: f64, characters: &[CombatSessionCharacterSummary]) -> Option<TeamDps> {
    (dps > 0.0).then(|| TeamDps {
        dps,
        members: characters
            .iter()
            .filter(|row| row.damage > 0.0)
            .take(crate::engine::model::TEAM_DPS_MAX_MEMBERS)
            .map(|row| TeamDpsMember {
                id: row.char_id,
                dps: row.dps,
                name: row.name.clone(),
            })
            .collect(),
    })
}

#[derive(Clone, Debug, Default)]
pub struct HistoryLoadResult {
    pub records: Vec<HistoryRecord>,
    pub skipped_files: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistoryComparison {
    pub left_id: String,
    pub right_id: String,
    pub total_dps_delta: f64,
    pub total_damage_delta: f64,
    pub duration_delta: f64,
    pub character_deltas: Vec<HistoryCharacterDelta>,
    pub skill_deltas: Vec<HistorySkillDelta>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistoryCharacterDelta {
    pub char_id: u32,
    pub name: String,
    pub left_dps: f64,
    pub right_dps: f64,
    pub delta_dps: f64,
    pub left_damage: f64,
    pub right_damage: f64,
    pub delta_damage: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistorySkillDelta {
    pub name: String,
    pub category: String,
    pub left_damage: f64,
    pub right_damage: f64,
    pub delta_damage: f64,
}

pub fn history_dir() -> PathBuf {
    software_dir().join("history")
}

fn software_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn load_history() -> HistoryLoadResult {
    load_history_from_dir(&history_dir())
}

pub fn load_history_from_dir(directory: &Path) -> HistoryLoadResult {
    let mut result = HistoryLoadResult::default();
    let Ok(entries) = fs::read_dir(directory) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        match fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| parse_history_record(&text, &path))
        {
            Ok(record) => result.records.push(record),
            Err(_) => result.skipped_files += 1,
        }
    }
    sort_records_newest_first(&mut result.records);
    result
}

pub fn save_summary(summary: CombatSessionSummary) -> Result<HistoryRecord, String> {
    save_summary_to_dir(&history_dir(), summary)
}

pub fn save_summary_to_dir(
    directory: &Path,
    summary: CombatSessionSummary,
) -> Result<HistoryRecord, String> {
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let saved_at = Utc::now();
    let id = generate_record_id(saved_at);
    let record = HistoryRecord {
        version: HISTORY_RECORD_VERSION,
        id,
        saved_at,
        summary,
    };
    let text = serde_json::to_string_pretty(&record).map_err(|error| error.to_string())?;
    atomic_write_text(&record_path(directory, &record), &format!("{text}\n"))?;
    prune_history_dir(directory, MAX_HISTORY_RECORDS)?;
    Ok(record)
}

pub fn delete_record(record_id: &str) -> Result<bool, String> {
    delete_record_from_dir(&history_dir(), record_id)
}

pub fn delete_record_from_dir(directory: &Path, record_id: &str) -> Result<bool, String> {
    if record_id.is_empty() {
        return Ok(false);
    }
    let mut deleted = false;
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if stem.ends_with(record_id) {
            fs::remove_file(&path).map_err(|error| error.to_string())?;
            deleted = true;
        }
    }
    Ok(deleted)
}

pub fn compare_records(left: &HistoryRecord, right: &HistoryRecord) -> HistoryComparison {
    let mut character_deltas =
        compare_characters(&left.summary.characters, &right.summary.characters);
    character_deltas.sort_by(|left, right| {
        right
            .delta_damage
            .abs()
            .total_cmp(&left.delta_damage.abs())
            .then_with(|| left.name.cmp(&right.name))
    });
    character_deltas.truncate(8);

    let mut skill_deltas = compare_skills(&left.summary.skills, &right.summary.skills);
    skill_deltas.sort_by(|left, right| {
        right
            .delta_damage
            .abs()
            .total_cmp(&left.delta_damage.abs())
            .then_with(|| left.name.cmp(&right.name))
    });
    skill_deltas.truncate(8);

    HistoryComparison {
        left_id: left.id.clone(),
        right_id: right.id.clone(),
        total_dps_delta: right.summary.total_dps - left.summary.total_dps,
        total_damage_delta: right.summary.total_damage - left.summary.total_damage,
        duration_delta: right.summary.duration_seconds - left.summary.duration_seconds,
        character_deltas,
        skill_deltas,
    }
}

fn parse_history_record(text: &str, path: &Path) -> Result<HistoryRecord, String> {
    let mut record: HistoryRecord =
        serde_json::from_str(text).map_err(|error| error.to_string())?;
    if record.version > HISTORY_RECORD_VERSION {
        return Err(format!("不支持的历史版本 {}", record.version));
    }
    if record.id.trim().is_empty() {
        record.id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("legacy")
            .to_owned();
    }
    Ok(record)
}

fn record_path(directory: &Path, record: &HistoryRecord) -> PathBuf {
    directory.join(format!(
        "{}_{}.json",
        record
            .saved_at
            .with_timezone(&Local)
            .format("%Y%m%d_%H%M%S"),
        record.id
    ))
}

fn prune_history_dir(directory: &Path, max_records: usize) -> Result<(), String> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| error.to_string())?;
        files.push((modified, path));
    }
    files.sort_by_key(|(modified, _)| *modified);
    let remove_count = files.len().saturating_sub(max_records);
    for (_, path) in files.into_iter().take(remove_count) {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn sort_records_newest_first(records: &mut [HistoryRecord]) {
    records.sort_by(|left, right| {
        right
            .saved_at
            .cmp(&left.saved_at)
            .then_with(|| right.id.cmp(&left.id))
    });
}

fn generate_record_id(saved_at: DateTime<Utc>) -> String {
    let counter = HISTORY_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = saved_at
        .timestamp_nanos_opt()
        .unwrap_or_else(|| saved_at.timestamp_millis().saturating_mul(1_000_000));
    let mut value = (nanos as u64) ^ ((std::process::id() as u64) << 32) ^ counter;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    format!("{:08x}", value & 0xffff_ffff)
}

fn compare_characters(
    left: &[CombatSessionCharacterSummary],
    right: &[CombatSessionCharacterSummary],
) -> Vec<HistoryCharacterDelta> {
    let mut rows = std::collections::HashMap::<u32, HistoryCharacterDelta>::new();
    for row in left {
        rows.insert(
            row.char_id,
            HistoryCharacterDelta {
                char_id: row.char_id,
                name: row.name.clone(),
                left_dps: row.dps,
                left_damage: row.damage,
                ..Default::default()
            },
        );
    }
    for row in right {
        let entry = rows
            .entry(row.char_id)
            .or_insert_with(|| HistoryCharacterDelta {
                char_id: row.char_id,
                name: row.name.clone(),
                ..Default::default()
            });
        if entry.name.is_empty() {
            entry.name.clone_from(&row.name);
        }
        entry.right_dps = row.dps;
        entry.right_damage = row.damage;
    }
    for row in rows.values_mut() {
        row.delta_dps = row.right_dps - row.left_dps;
        row.delta_damage = row.right_damage - row.left_damage;
    }
    rows.into_values().collect()
}

fn compare_skills(
    left: &[CombatSessionSkillSummary],
    right: &[CombatSessionSkillSummary],
) -> Vec<HistorySkillDelta> {
    let mut rows = std::collections::HashMap::<(String, String), HistorySkillDelta>::new();
    for row in left {
        let key = (row.name.clone(), row.category.clone());
        let entry = rows.entry(key).or_insert_with(|| HistorySkillDelta {
            name: row.name.clone(),
            category: row.category.clone(),
            ..Default::default()
        });
        entry.left_damage += row.damage;
    }
    for row in right {
        let key = (row.name.clone(), row.category.clone());
        let entry = rows.entry(key).or_insert_with(|| HistorySkillDelta {
            name: row.name.clone(),
            category: row.category.clone(),
            ..Default::default()
        });
        entry.right_damage += row.damage;
    }
    for row in rows.values_mut() {
        row.delta_damage = row.right_damage - row.left_damage;
    }
    rows.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::{
        CombatSessionAbyssHalfSummary, CombatSessionAbyssSummary, CombatSessionCharacterSummary,
        CombatSessionSummary,
    };

    #[test]
    fn loads_legacy_version_record() {
        let directory = temp_history_dir("legacy");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("legacy.json"),
            r#"{"version":0,"id":"","saved_at":"2026-01-01T00:00:00Z","summary":{"total_damage":100.0}}"#,
        )
        .unwrap();

        let result = load_history_from_dir(&directory);

        assert_eq!(result.skipped_files, 0);
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].version, 0);
        assert!(!result.records[0].id.is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn skips_corrupt_json() {
        let directory = temp_history_dir("corrupt");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("bad.json"), "{not json").unwrap();

        let result = load_history_from_dir(&directory);

        assert_eq!(result.records.len(), 0);
        assert_eq!(result.skipped_files, 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn prunes_oldest_records() {
        let directory = temp_history_dir("prune");
        fs::create_dir_all(&directory).unwrap();
        for index in 0..3 {
            let mut summary = CombatSessionSummary {
                total_damage: index as f64,
                ..Default::default()
            };
            summary.characters.push(CombatSessionCharacterSummary {
                char_id: index,
                name: format!("角色{index}"),
                damage: index as f64,
                dps: index as f64,
                ..Default::default()
            });
            save_summary_to_dir(&directory, summary).unwrap();
        }

        prune_history_dir(&directory, 2).unwrap();
        let files = fs::read_dir(&directory).unwrap().count();

        assert_eq!(files, 2);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn abyss_record_labels_and_prediction_teams_use_each_half() {
        let record = HistoryRecord {
            summary: CombatSessionSummary {
                total_dps: 999.0,
                characters: vec![
                    character(1, "上角色", 100.0, 10.0),
                    character(2, "下角色", 200.0, 20.0),
                ],
                abyss: CombatSessionAbyssSummary {
                    detected: true,
                    first_half: Some(CombatSessionAbyssHalfSummary {
                        half: "上行线".to_owned(),
                        total_dps: 10.0,
                        characters: vec![character(1, "上角色", 100.0, 10.0)],
                        ..Default::default()
                    }),
                    second_half: Some(CombatSessionAbyssHalfSummary {
                        half: "下行线".to_owned(),
                        total_dps: 20.0,
                        characters: vec![character(2, "下角色", 200.0, 20.0)],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(record.upper_team_dps().unwrap().members[0].id, 1);
        assert_eq!(record.lower_team_dps().unwrap().members[0].id, 2);
    }

    #[test]
    fn compare_records_aggregates_duplicate_skill_rows() {
        let left = HistoryRecord {
            id: "left".to_owned(),
            summary: CombatSessionSummary {
                skills: vec![
                    skill("待映射技能", "未知", 100.0),
                    skill("待映射技能", "未知", 25.0),
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let right = HistoryRecord {
            id: "right".to_owned(),
            summary: CombatSessionSummary {
                skills: vec![
                    skill("待映射技能", "未知", 10.0),
                    skill("待映射技能", "未知", 5.0),
                ],
                ..Default::default()
            },
            ..Default::default()
        };

        let comparison = compare_records(&left, &right);

        assert_eq!(comparison.skill_deltas.len(), 1);
        let delta = &comparison.skill_deltas[0];
        assert_eq!(delta.left_damage, 125.0);
        assert_eq!(delta.right_damage, 15.0);
        assert_eq!(delta.delta_damage, -110.0);
    }

    fn temp_history_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nte_history_test_{}_{}_{}",
            name,
            std::process::id(),
            HISTORY_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn character(char_id: u32, name: &str, damage: f64, dps: f64) -> CombatSessionCharacterSummary {
        CombatSessionCharacterSummary {
            char_id,
            name: name.to_owned(),
            damage,
            dps,
            ..Default::default()
        }
    }

    fn skill(name: &str, category: &str, damage: f64) -> CombatSessionSkillSummary {
        CombatSessionSkillSummary {
            name: name.to_owned(),
            category: category.to_owned(),
            damage,
            ..Default::default()
        }
    }
}
