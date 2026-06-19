use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::parser::find_data_file;

const TARGET_RESOURCE_DIR: &str = "res/data/targets";
const TARGET_RESOURCE_FILES: [&str; 5] = [
    "monster_mapping.json",
    "boss_mapping.json",
    "name_overrides.json",
    "class_path_rules.json",
    "localization_index.json",
];
const MONSTER_MANUAL_TABLE_FILES: [&str; 2] = [
    "data/DataTable/DT_MonsterManualConfig.json",
    "NTE_Assets/DataTable/DT_MonsterManualConfig.json",
];
const MONSTER_STATIC_TABLE_FILES: [&str; 10] = [
    "data/DataTable/Monster/DT_MonsterStaticData_Abyss.json",
    "data/DataTable/Monster/DT_MonsterStaticData_BigWorld.json",
    "data/DataTable/Monster/DT_MonsterStaticData_BigWorld_Gameplay.json",
    "data/DataTable/Monster/DT_MonsterStaticData_BigWorld_Quest.json",
    "data/DataTable/Monster/DT_MonsterStaticData_Clone.json",
    "NTE_Assets/DataTable/Monster/DT_MonsterStaticData_Abyss.json",
    "NTE_Assets/DataTable/Monster/DT_MonsterStaticData_BigWorld.json",
    "NTE_Assets/DataTable/Monster/DT_MonsterStaticData_BigWorld_Gameplay.json",
    "NTE_Assets/DataTable/Monster/DT_MonsterStaticData_BigWorld_Quest.json",
    "NTE_Assets/DataTable/Monster/DT_MonsterStaticData_Clone.json",
];
const MONSTER_PACK_TABLE_FILES: [&str; 2] = [
    "data/DataTable/PackData/DT_MonsterPackData.json",
    "NTE_Assets/DataTable/PackData/DT_MonsterPackData.json",
];
const LACRIMOSA_COLLECTION_TABLE_FILES: [&str; 2] = [
    "data/DataTable/LacrimosaCollection/DT_LacrimosaCollectionData.json",
    "NTE_Assets/DataTable/LacrimosaCollection/DT_LacrimosaCollectionData.json",
];

const PRIORITY_OVERRIDE: u8 = 250;
const PRIORITY_ABYSS_STAGE: u8 = 230;
const PRIORITY_MANUAL: u8 = 220;
const PRIORITY_LACRIMOSA_COLLECTION: u8 = 210;
const PRIORITY_STATIC_TEXT: u8 = 180;
const PRIORITY_STATIC_COMMENT: u8 = 100;

#[derive(Clone, Debug, Default)]
pub struct ResourceIndex {
    names_by_path: HashMap<String, NameEntry>,
    canonical_targets_by_path: HashMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NameEntry {
    name: String,
    priority: u8,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TargetDocument {
    Map(HashMap<String, String>),
    Rows {
        #[serde(alias = "Rows")]
        rows: HashMap<String, String>,
    },
    List(Vec<TargetRow>),
}

#[derive(Deserialize)]
struct TargetRow {
    #[serde(default)]
    class_path: String,
    #[serde(default)]
    object_path: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Clone, Debug)]
struct ManualMonsterRow {
    row_id: String,
    name: String,
    aliases: Vec<String>,
    drop_id: Option<String>,
}

impl ResourceIndex {
    #[allow(dead_code)]
    pub fn load_default() -> Self {
        let mut warnings = Vec::new();
        Self::load_default_with_warnings(&mut warnings)
    }

    pub fn load_default_with_warnings(warnings: &mut Vec<String>) -> Self {
        let mut index = Self::default();
        let initial_warning_count = warnings.len();
        let mut missing_groups = Vec::new();
        let mut loaded_manual = 0usize;
        for relative in MONSTER_MANUAL_TABLE_FILES {
            let Some(path) = find_data_file(Path::new(relative)) else {
                continue;
            };
            loaded_manual += 1;
            index.load_monster_manual_file_with_warnings(&path, warnings);
        }
        if loaded_manual == 0 {
            missing_groups.push("DT_MonsterManualConfig");
        }
        let mut loaded_static = 0usize;
        for relative in MONSTER_STATIC_TABLE_FILES {
            let Some(path) = find_data_file(Path::new(relative)) else {
                continue;
            };
            loaded_static += 1;
            index.load_monster_static_file_with_warnings(&path, warnings);
        }
        if loaded_static == 0 {
            missing_groups.push("DT_MonsterStaticData");
        }
        let mut loaded_pack = 0usize;
        for relative in MONSTER_PACK_TABLE_FILES {
            let Some(path) = find_data_file(Path::new(relative)) else {
                continue;
            };
            loaded_pack += 1;
            index.load_monster_pack_file_with_warnings(&path, warnings);
        }
        if loaded_pack == 0 {
            missing_groups.push("DT_MonsterPackData");
        }
        let mut loaded_lacrimosa = 0usize;
        for relative in LACRIMOSA_COLLECTION_TABLE_FILES {
            let Some(path) = find_data_file(Path::new(relative)) else {
                continue;
            };
            loaded_lacrimosa += 1;
            index.load_lacrimosa_collection_file_with_warnings(&path, warnings);
        }
        if loaded_lacrimosa == 0 {
            missing_groups.push("DT_LacrimosaCollectionData");
        }
        for file in TARGET_RESOURCE_FILES {
            let relative = Path::new(TARGET_RESOURCE_DIR).join(file);
            let Some(path) = find_data_file(&relative) else {
                warnings.push(format!("missing target resource {}", relative.display()));
                continue;
            };
            index.load_file_with_warnings(&path, warnings);
        }
        if !missing_groups.is_empty() {
            warnings.push(format!(
                "missing target DataTable groups: {}",
                missing_groups.join(", ")
            ));
        }
        warnings.push(format!(
            "target resource index loaded: names_by_path={} canonical_targets_by_path={} warnings_added={}",
            index.names_by_path.len(),
            index.canonical_targets_by_path.len(),
            warnings.len().saturating_sub(initial_warning_count)
        ));
        index
    }

    #[cfg(test)]
    pub(crate) fn insert_test_target(&mut self, path: &str, name: &str) {
        self.insert(path.to_owned(), name.to_owned(), PRIORITY_OVERRIDE);
    }

    pub fn display_name_for_path(&self, path: &str) -> Option<String> {
        self.resolved_name_for_path(path)
            .or_else(|| fallback_name_from_path(path))
    }

    pub fn resolved_name_for_path(&self, path: &str) -> Option<String> {
        lookup_keys_for_path(path)
            .into_iter()
            .filter_map(|key| self.names_by_path.get(&key))
            .max_by_key(|entry| entry.priority)
            .map(|entry| entry.name.clone())
    }

    pub fn canonical_target_path_for_path(&self, path: &str) -> Option<String> {
        lookup_keys_for_path(path)
            .into_iter()
            .find_map(|key| self.canonical_targets_by_path.get(&key).cloned())
    }

    pub fn resolved_name_for_gameplay_effect(&self, effect_name: &str) -> Option<String> {
        lookup_keys_for_gameplay_effect(effect_name)
            .into_iter()
            .filter_map(|key| self.names_by_path.get(&key))
            .max_by_key(|entry| entry.priority)
            .map(|entry| entry.name.clone())
    }

    pub fn canonical_target_path_for_gameplay_effect(&self, effect_name: &str) -> Option<String> {
        lookup_keys_for_gameplay_effect(effect_name)
            .into_iter()
            .find_map(|key| self.canonical_targets_by_path.get(&key).cloned())
    }

    pub(crate) fn load_file_with_warnings(&mut self, path: &Path, warnings: &mut Vec<String>) {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                warnings.push(format!(
                    "target resource {} read failed: {error}",
                    path.display()
                ));
                return;
            }
        };
        let document = match serde_json::from_str::<TargetDocument>(&text) {
            Ok(document) => document,
            Err(error) => {
                warnings.push(format!(
                    "target resource {} has unsupported JSON structure: {error}",
                    path.display()
                ));
                return;
            }
        };
        match document {
            TargetDocument::Map(rows) | TargetDocument::Rows { rows } => {
                for (path, name) in rows {
                    self.insert(path, name, PRIORITY_OVERRIDE);
                }
            }
            TargetDocument::List(rows) => {
                for row in rows {
                    let path = first_non_empty([row.path, row.class_path, row.object_path]);
                    let name = first_non_empty([row.display_name, row.name, String::new()]);
                    self.insert(path, name, PRIORITY_OVERRIDE);
                }
            }
        }
    }

    fn load_monster_manual_file_with_warnings(&mut self, path: &Path, warnings: &mut Vec<String>) {
        let Some(rows) = read_data_table_rows(path, warnings) else {
            return;
        };
        let mut manual_rows = Vec::new();
        for (row_id, row) in rows {
            let Some(name) = localized_text(row.get("MonsterName")) else {
                continue;
            };
            let mut aliases = Vec::new();
            for field in ["WorldBossID", "CloneID", "CloneEnterID", "MonsterTag"] {
                if let Some(alias) = row
                    .get(field)
                    .and_then(Value::as_str)
                    .and_then(non_empty_text)
                {
                    aliases.push(alias);
                }
            }
            let drop_id = row
                .get("DropID")
                .and_then(Value::as_str)
                .and_then(non_empty_text);
            manual_rows.push(ManualMonsterRow {
                row_id,
                name,
                aliases,
                drop_id,
            });
        }

        let drop_key_counts = manual_drop_key_counts(&manual_rows);
        for row in manual_rows {
            self.insert_monster_id(&row.row_id, &row.name, PRIORITY_MANUAL);
            for alias in row.aliases {
                self.insert_monster_id(&alias, &row.name, PRIORITY_MANUAL);
            }
            if let Some(drop_id) = row.drop_id
                && is_unique_manual_drop_id(&drop_id, &drop_key_counts)
            {
                self.insert_monster_id(&drop_id, &row.name, PRIORITY_MANUAL);
                self.insert_canonical_target_alias(&drop_id, &row.row_id);
            }
        }
    }

    fn load_monster_static_file_with_warnings(&mut self, path: &Path, warnings: &mut Vec<String>) {
        let Some(rows) = read_data_table_rows(path, warnings) else {
            return;
        };
        for (row_id, row) in rows {
            if let Some(name) = localized_text(row.get("TextName")) {
                self.insert_monster_id(&row_id, &name, PRIORITY_STATIC_TEXT);
            } else if let Some(name) = row
                .get("Comment")
                .and_then(Value::as_str)
                .and_then(comment_name)
            {
                self.insert_monster_id(&row_id, &name, PRIORITY_STATIC_COMMENT);
            }

            for tag in row
                .get("Tags")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if let Some(name) = self.display_name_for_path(&row_id) {
                    self.insert_monster_id(tag, &name, PRIORITY_STATIC_TEXT);
                }
            }
        }
    }

    fn load_monster_pack_file_with_warnings(&mut self, path: &Path, warnings: &mut Vec<String>) {
        let Some(rows) = read_data_table_rows(path, warnings) else {
            return;
        };
        for row_id in rows.keys() {
            let Some((stage_id, monster_id)) = abyss_pack_target(row_id) else {
                continue;
            };
            let Some(name) = self.display_name_for_path(&monster_id) else {
                continue;
            };
            self.insert_monster_id(&stage_id, &name, PRIORITY_ABYSS_STAGE);
            self.insert_monster_id(row_id, &name, PRIORITY_ABYSS_STAGE);
            self.insert_canonical_target_alias(&stage_id, &monster_id);
            self.insert_canonical_target_alias(row_id, &monster_id);
        }
    }

    fn load_lacrimosa_collection_file_with_warnings(
        &mut self,
        path: &Path,
        warnings: &mut Vec<String>,
    ) {
        let Some(rows) = read_data_table_rows(path, warnings) else {
            return;
        };
        for (row_id, row) in rows {
            let Some(table_name) = localized_text(row.get("MonsterName")) else {
                continue;
            };
            let ability_id = row
                .get("AbilityId")
                .and_then(Value::as_str)
                .and_then(non_empty_text)
                .unwrap_or(row_id);
            let ability_path = nested_asset_path(
                &row,
                &[
                    "StolenAbilityCollection",
                    "AbilityCfg",
                    "AbilityClass",
                    "AssetPathName",
                ],
            );
            let target_path = nested_asset_path(
                &row,
                &[
                    "StolenAbilityCollection",
                    "TargetAvatarClass",
                    "AssetPathName",
                ],
            );
            let name = target_path
                .as_deref()
                .and_then(|path| self.resolved_name_for_path(path))
                .unwrap_or(table_name);

            self.insert_monster_id(&ability_id, &name, PRIORITY_LACRIMOSA_COLLECTION);
            if let Some(target_path) = &target_path {
                if self.resolved_name_for_path(target_path).is_none() {
                    self.insert(
                        target_path.clone(),
                        name.clone(),
                        PRIORITY_LACRIMOSA_COLLECTION,
                    );
                }
                self.insert_canonical_target_alias(&ability_id, target_path);
                if let Some(ability_path) = &ability_path {
                    self.insert_canonical_target_alias(ability_path, target_path);
                }
            }

            for alias in monster_namespace_aliases(&ability_id).into_iter().chain(
                ability_path
                    .as_deref()
                    .map(monster_namespace_aliases)
                    .unwrap_or_default(),
            ) {
                self.insert_monster_id(&alias, &name, PRIORITY_LACRIMOSA_COLLECTION);
                if let Some(target_path) = &target_path {
                    self.insert_canonical_target_alias(&alias, target_path);
                }
            }
        }
    }

    fn insert(&mut self, path: String, name: String, priority: u8) {
        let aliases = lookup_keys_for_path(&path);
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        for alias in aliases {
            self.insert_key(alias, name, priority);
        }
    }

    fn insert_monster_id(&mut self, id: &str, name: &str, priority: u8) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let mut aliases = Vec::new();
        add_identifier_aliases(clean_identifier(id).as_str(), &mut aliases);
        for alias in aliases {
            self.insert_key(alias, name, priority);
        }
    }

    fn insert_canonical_target_alias(&mut self, alias: &str, target: &str) {
        let target = clean_identifier(target);
        if target.is_empty() {
            return;
        }
        for key in lookup_keys_for_path(alias) {
            self.canonical_targets_by_path
                .entry(key)
                .or_insert_with(|| target.clone());
        }
    }

    fn insert_key(&mut self, key: String, name: &str, priority: u8) {
        let key = normalize_key(&key);
        if key.is_empty() {
            return;
        }
        match self.names_by_path.get(&key) {
            Some(existing) if existing.priority >= priority => {}
            _ => {
                self.names_by_path.insert(
                    key,
                    NameEntry {
                        name: name.to_owned(),
                        priority,
                    },
                );
            }
        }
    }
}

fn first_non_empty(values: impl IntoIterator<Item = String>) -> String {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

fn abyss_pack_target(row_id: &str) -> Option<(String, String)> {
    let parts = row_id.split('_').collect::<Vec<_>>();
    if parts.len() < 8 || parts.first().copied() != Some("Abyss") {
        return None;
    }
    if !parts[1..5]
        .iter()
        .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    let monster_start = 5;
    let monster_prefix = parts.get(monster_start)?.to_ascii_lowercase();
    if monster_prefix != "boss" && monster_prefix != "mon" {
        return None;
    }
    let monster_id = parts[monster_start..].join("_");
    if !monster_id.to_ascii_lowercase().contains("_bp") {
        return None;
    }
    Some((parts[..4].join("_"), monster_id))
}

fn read_data_table_rows(path: &Path, warnings: &mut Vec<String>) -> Option<Map<String, Value>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            warnings.push(format!(
                "target resource {} read failed: {error}",
                path.display()
            ));
            return None;
        }
    };
    let value = match serde_json::from_str::<Value>(&text) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(format!(
                "target resource {} has unsupported JSON structure: {error}",
                path.display()
            ));
            return None;
        }
    };
    data_table_rows(&value).cloned().or_else(|| {
        warnings.push(format!(
            "target resource {} has no DataTable Rows object",
            path.display()
        ));
        None
    })
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

fn nested_asset_path(row: &Value, fields: &[&str]) -> Option<String> {
    let mut value = row;
    for field in fields {
        value = value.get(*field)?;
    }
    value.as_str().and_then(non_empty_text)
}

fn manual_drop_key_counts(rows: &[ManualMonsterRow]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        let Some(drop_id) = &row.drop_id else {
            continue;
        };
        for key in lookup_keys_for_path(drop_id) {
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    counts
}

fn is_unique_manual_drop_id(drop_id: &str, counts: &HashMap<String, usize>) -> bool {
    let keys = lookup_keys_for_path(drop_id);
    !keys.is_empty() && keys.iter().all(|key| counts.get(key).copied() == Some(1))
}

fn localized_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return non_empty_text(text);
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
    .find_map(non_empty_text)
}

fn non_empty_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value != "None").then(|| value.to_owned())
}

fn comment_name(comment: &str) -> Option<String> {
    let trimmed = comment.trim();
    if !contains_cjk(trimmed) {
        return None;
    }
    let mut candidate = trimmed;
    for delimiter in ['-', '－', '—', ':', '：'] {
        if let Some((_, right)) = candidate.rsplit_once(delimiter)
            && contains_cjk(right)
        {
            candidate = right.trim();
        }
    }
    for prefix in [
        "角色试用副本",
        "试用关卡",
        "自定义BOSS",
        "世界Boss",
        "异象委托",
        "副本",
    ] {
        candidate = candidate.trim_start_matches(prefix).trim();
    }
    let candidate = candidate.trim_matches(['-', '－', '—', ':', '：', ' ']);
    (!candidate.is_empty() && contains_cjk(candidate)).then(|| candidate.to_owned())
}

fn contains_cjk(value: &str) -> bool {
    value
        .chars()
        .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character))
}

fn lookup_keys_for_path(path: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    let clean = clean_identifier(path);
    add_identifier_aliases(&clean, &mut aliases);

    let after_slash = clean.rsplit('/').next().unwrap_or(&clean);
    add_identifier_aliases(after_slash, &mut aliases);
    if let Some((left, right)) = after_slash.rsplit_once('.') {
        add_identifier_aliases(left, &mut aliases);
        add_identifier_aliases(right, &mut aliases);
    }

    let mut seen = HashSet::new();
    aliases
        .into_iter()
        .map(|alias| normalize_key(&alias))
        .filter(|alias| !alias.is_empty() && seen.insert(alias.clone()))
        .collect()
}

fn lookup_keys_for_gameplay_effect(effect_name: &str) -> Vec<String> {
    let mut aliases = lookup_keys_for_path(effect_name);
    for namespace in monster_namespace_aliases(effect_name) {
        aliases.extend(lookup_keys_for_path(&namespace));
    }
    let mut seen = HashSet::new();
    aliases
        .into_iter()
        .filter(|alias| !alias.is_empty() && seen.insert(alias.clone()))
        .collect()
}

fn add_identifier_aliases(value: &str, aliases: &mut Vec<String>) {
    let clean = clean_identifier(value);
    if clean.is_empty() {
        return;
    }
    push_alias(aliases, &clean);

    if let Some(stripped) = strip_class_suffix(&clean) {
        push_alias(aliases, &stripped);
    }
    if let Some(stripped) = strip_instance_suffix(&clean) {
        push_alias(aliases, &stripped);
        if let Some(class_stripped) = strip_class_suffix(&stripped) {
            push_alias(aliases, &class_stripped);
        }
    }
    if let Some(without_default) = clean.strip_prefix("Default__") {
        add_identifier_aliases(without_default, aliases);
    }
    if let Some(remapped) = remap_bp_prefix(&clean) {
        add_identifier_aliases(&remapped, aliases);
    }
    if let Some(remapped) = remap_world_boss(&clean) {
        add_identifier_aliases(&remapped, aliases);
    }
    if let Some(remapped) = remap_weekly_clone_boss(&clean) {
        add_identifier_aliases(&remapped, aliases);
    }
    for stripped in strip_monster_variants(&clean) {
        add_identifier_aliases(&stripped, aliases);
    }
    for remapped in remap_monster_phase_to_base_bp(&clean) {
        add_identifier_aliases(&remapped, aliases);
    }
    for remapped in remap_bare_monster_number_to_bp(&clean) {
        add_identifier_aliases(&remapped, aliases);
    }
    for normalized in normalize_monster_number_variants(&clean) {
        push_alias(aliases, &normalized);
    }
}

fn push_alias(aliases: &mut Vec<String>, value: &str) {
    let key = normalize_key(value);
    if !key.is_empty() {
        aliases.push(key);
    }
}

fn clean_identifier(value: &str) -> String {
    value
        .trim_matches(|character: char| character == '\0' || character.is_control())
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('\0')
        .to_owned()
}

fn normalize_key(value: &str) -> String {
    clean_identifier(value).to_ascii_lowercase()
}

fn monster_namespace_aliases(value: &str) -> Vec<String> {
    let lower = value.to_ascii_lowercase();
    let mut aliases = Vec::new();
    for prefix in ["boss", "mon"] {
        let marker = format!("{prefix}_");
        let mut search_start = 0;
        while let Some(relative_index) = lower[search_start..].find(&marker) {
            let digit_start = search_start + relative_index + marker.len();
            let digits = lower[digit_start..]
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>();
            if let Ok(number) = digits.parse::<u32>() {
                push_monster_namespace_aliases(&mut aliases, prefix, number);
            }
            search_start = digit_start + digits.len().max(1);
            if search_start >= lower.len() {
                break;
            }
        }
    }
    aliases
}

fn push_monster_namespace_aliases(aliases: &mut Vec<String>, prefix: &str, number: u32) {
    for alias in [
        format!("{prefix}_{number}"),
        format!("{prefix}_{number:02}"),
        format!("{prefix}_{number:03}"),
        format!("{prefix}_{number}_BP"),
        format!("{prefix}_{number:02}_BP"),
        format!("{prefix}_{number:03}_BP"),
    ] {
        if !aliases.iter().any(|existing| existing == &alias) {
            aliases.push(alias);
        }
    }
}

fn strip_class_suffix(value: &str) -> Option<String> {
    if let Some(stripped) = value.strip_suffix("_C") {
        return Some(stripped.to_owned());
    }
    let (head, tail) = value.rsplit_once("_C_")?;
    (tail.len() >= 3 && tail.bytes().all(|byte| byte.is_ascii_digit())).then(|| head.to_owned())
}

fn strip_instance_suffix(value: &str) -> Option<String> {
    let (head, tail) = value.rsplit_once('_')?;
    (tail.len() >= 6 && tail.bytes().all(|byte| byte.is_ascii_digit())).then(|| head.to_owned())
}

fn remap_bp_prefix(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("bp_boss_") {
        let suffix = value.get(8..)?;
        return Some(format!("boss_{suffix}_BP"));
    }
    if lower.starts_with("bp_mon_") {
        let suffix = value.get(7..)?;
        return Some(format!("mon_{suffix}_BP"));
    }
    None
}

fn remap_world_boss(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("worldboss_") {
        let digits = value
            .chars()
            .filter(|character| character.is_ascii_digit())
            .collect::<String>();
        if let Ok(number) = digits.parse::<u32>() {
            return Some(format!("boss_{number:02}_BP"));
        }
    }
    if lower.starts_with("boss_") && lower.ends_with("_worldboss") {
        return value
            .get(..value.len().saturating_sub("_WorldBoss".len()))
            .map(str::to_owned);
    }
    None
}

fn remap_weekly_clone_boss(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let suffix = lower.strip_prefix("weeklyclone_boss")?;
    let digits = suffix
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    let number = digits.parse::<u32>().ok()?;
    Some(format!("boss_{number:02}_BP"))
}

fn remap_bare_monster_number_to_bp(value: &str) -> Vec<String> {
    let Some((prefix, (number, suffix))) = split_monster_number(value) else {
        return Vec::new();
    };
    if !suffix.is_empty() {
        return Vec::new();
    }
    [
        format!("{prefix}_{number}_BP"),
        format!("{prefix}_{number:02}_BP"),
        format!("{prefix}_{number:03}_BP"),
    ]
    .into_iter()
    .collect()
}

fn remap_monster_phase_to_base_bp(value: &str) -> Vec<String> {
    let Some((prefix, (number, suffix))) = split_monster_number(value) else {
        return Vec::new();
    };
    let Some(after_phase) = suffix.strip_prefix('_') else {
        return Vec::new();
    };
    let phase_digits = after_phase
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if phase_digits == 0 {
        return Vec::new();
    }
    let after_phase = &after_phase[phase_digits..];
    if !after_phase.to_ascii_lowercase().starts_with("_bp") {
        return Vec::new();
    }
    [
        format!("{prefix}_{number}_BP"),
        format!("{prefix}_{number:02}_BP"),
        format!("{prefix}_{number:03}_BP"),
    ]
    .into_iter()
    .collect()
}

fn strip_monster_variants(value: &str) -> Vec<String> {
    let lower = value.to_ascii_lowercase();
    let mut aliases = Vec::new();
    if let Some(index) = lower.find("_bp_") {
        aliases.push(value[..index + 3].to_owned());
    }
    for suffix in [
        "_trial",
        "_world",
        "_gameplay",
        "_quest",
        "_clone",
        "_abyss",
        "_diyboss",
        "_vision",
        "_takeorder",
        "_weekly",
    ] {
        if let Some(index) = lower.find(suffix) {
            aliases.push(value[..index].to_owned());
        }
    }
    aliases
}

fn normalize_monster_number_variants(value: &str) -> Vec<String> {
    let Some((prefix, rest)) = split_monster_number(value) else {
        return Vec::new();
    };
    let (number, suffix) = rest;
    [
        format!("{prefix}_{number}{suffix}"),
        format!("{prefix}_{number:02}{suffix}"),
        format!("{prefix}_{number:03}{suffix}"),
    ]
    .into_iter()
    .collect()
}

fn split_monster_number(value: &str) -> Option<(&'static str, (u32, &str))> {
    let lower = value.to_ascii_lowercase();
    let prefix = if lower.starts_with("mon_") {
        "mon"
    } else if lower.starts_with("boss_") {
        "boss"
    } else {
        return None;
    };
    let rest = &value[prefix.len() + 1..];
    let digit_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_len == 0 {
        return None;
    }
    let number = rest[..digit_len].parse::<u32>().ok()?;
    Some((prefix, (number, &rest[digit_len..])))
}

pub fn fallback_name_from_path(path: &str) -> Option<String> {
    let trimmed = clean_identifier(path);
    if trimmed.is_empty() {
        return None;
    }
    let without_class = trimmed
        .rsplit('/')
        .next()
        .unwrap_or(&trimmed)
        .rsplit('.')
        .next()
        .unwrap_or(&trimmed)
        .trim_end_matches("_C")
        .trim_matches('\0');
    if without_class.len() < 3 {
        return None;
    }
    Some(without_class.to_owned())
}
