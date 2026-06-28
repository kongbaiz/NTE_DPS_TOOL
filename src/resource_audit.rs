use std::collections::{HashMap, HashSet};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use serde_json::Value;

use crate::parser::{
    CHARACTER_DATA_PATH, GAMEPLAY_EFFECT_MAPPING_PATH, SKILL_DAMAGE_DATA_PATH,
    WOODEN_DAMAGE_DESCRIPTIONS_PATH,
};
use crate::resource::{read_resource_text, resource_exists};

const ABYSS_MONSTERS_PATH: &str = "res/data/abyss/abyss_monsters.json";
const REACTIONS_PATH: &str = "res/data/reactions/reactions.json";
const DAMAGE_DIGIT_IMAGE_DIR: &str = "res/images/font/tiaozi1";
const MONSTER_IMAGE_DIR: &str = "res/images/monsters";
const REACTION_TEXT_IMAGE_COUNT: u8 = 8;

const ATTRIBUTE_ICON_PATHS: [(&str, &str); 6] = [
    ("灵", "res/images/attributes/UI_avatarbg_Icon_01.png"),
    ("咒", "res/images/attributes/UI_avatarbg_Icon_06.png"),
    ("光", "res/images/attributes/UI_avatarbg_Icon_04.png"),
    ("魂", "res/images/attributes/UI_avatarbg_Icon_05.png"),
    ("暗", "res/images/attributes/UI_avatarbg_Icon_03.png"),
    ("相", "res/images/attributes/UI_avatarbg_Icon_02.png"),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ResourceAuditSeverity {
    Error,
    #[default]
    Warning,
}

impl ResourceAuditSeverity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "错误",
            Self::Warning => "警告",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warning => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ResourceAuditCategory {
    #[default]
    Character,
    Skill,
    GameplayEffect,
    Abyss,
    Reaction,
    File,
}

impl ResourceAuditCategory {
    pub fn all() -> &'static [Self] {
        &[
            Self::Character,
            Self::Skill,
            Self::GameplayEffect,
            Self::Abyss,
            Self::Reaction,
            Self::File,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Character => "角色",
            Self::Skill => "技能",
            Self::GameplayEffect => "GE",
            Self::Abyss => "深渊",
            Self::Reaction => "反应",
            Self::File => "文件",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceAuditItem {
    pub severity: ResourceAuditSeverity,
    pub category: ResourceAuditCategory,
    pub resource_id: String,
    pub display_name: String,
    pub message: String,
    pub suggested_source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceAuditCounts {
    pub characters: usize,
    pub skill_damage: usize,
    pub mapped_effects: usize,
    pub wooden_names: usize,
    pub abyss_monsters: usize,
    pub reactions: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceAuditSummary {
    pub counts: ResourceAuditCounts,
    pub items: Vec<ResourceAuditItem>,
}

impl ResourceAuditSummary {
    pub fn error_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.severity == ResourceAuditSeverity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.severity == ResourceAuditSeverity::Warning)
            .count()
    }

    pub fn redacted_text(&self) -> String {
        let mut text = String::new();
        text.push_str("NTE DPS TOOL 资源覆盖率报告\n");
        text.push_str(&format!(
            "角色 {}，技能 {}，GE 映射 {}，中文名 {}，深渊怪物 {}，反应 {}\n",
            self.counts.characters,
            self.counts.skill_damage,
            self.counts.mapped_effects,
            self.counts.wooden_names,
            self.counts.abyss_monsters,
            self.counts.reactions
        ));
        text.push_str(&format!(
            "错误 {}，警告 {}\n",
            self.error_count(),
            self.warning_count()
        ));
        for item in self.items.iter().take(80) {
            text.push_str(&format!(
                "[{}][{}] {} {} - {}（建议来源：{}）\n",
                item.severity.label(),
                item.category.label(),
                item.resource_id,
                item.display_name,
                item.message,
                item.suggested_source
            ));
        }
        if self.items.len() > 80 {
            text.push_str(&format!("另有 {} 条未列出\n", self.items.len() - 80));
        }
        text.trim_end().to_owned()
    }
}

trait ResourceReader {
    fn read_text(&self, relative_path: &str) -> Result<String, String>;
    fn exists(&self, relative_path: &str) -> bool;
}

struct RuntimeResourceReader;

impl ResourceReader for RuntimeResourceReader {
    fn read_text(&self, relative_path: &str) -> Result<String, String> {
        read_resource_text(Path::new(relative_path)).map_err(|error| error.to_string())
    }

    fn exists(&self, relative_path: &str) -> bool {
        resource_exists(Path::new(relative_path))
    }
}

#[cfg(test)]
struct FsResourceReader {
    root: PathBuf,
}

#[cfg(test)]
impl ResourceReader for FsResourceReader {
    fn read_text(&self, relative_path: &str) -> Result<String, String> {
        std::fs::read_to_string(self.root.join(relative_path))
            .map_err(|error| format!("{relative_path}: {error}"))
    }

    fn exists(&self, relative_path: &str) -> bool {
        self.root.join(relative_path).is_file()
    }
}

pub fn audit_runtime_resources() -> ResourceAuditSummary {
    audit_with_reader(&RuntimeResourceReader)
}

#[cfg(test)]
fn audit_resource_root(root: &Path) -> ResourceAuditSummary {
    audit_with_reader(&FsResourceReader {
        root: root.to_path_buf(),
    })
}

fn audit_with_reader(reader: &dyn ResourceReader) -> ResourceAuditSummary {
    let mut audit = ResourceAuditSummary::default();
    audit_characters(reader, &mut audit);
    audit_skills(reader, &mut audit);
    audit_abyss_monsters(reader, &mut audit);
    audit_reactions(reader, &mut audit);
    audit.items.sort_by(|left, right| {
        left.severity
            .rank()
            .cmp(&right.severity.rank())
            .then_with(|| left.category.label().cmp(right.category.label()))
            .then_with(|| left.resource_id.cmp(&right.resource_id))
            .then_with(|| left.message.cmp(&right.message))
    });
    audit
}

fn audit_characters(reader: &dyn ResourceReader, audit: &mut ResourceAuditSummary) {
    let Some(document) = read_json_object(reader, CHARACTER_DATA_PATH, audit) else {
        return;
    };
    let Some(characters) = document.get("characters").and_then(Value::as_object) else {
        push_item(
            audit,
            ResourceAuditSeverity::Error,
            ResourceAuditCategory::Character,
            CHARACTER_DATA_PATH,
            "characters",
            "缺少 characters 对象",
            CHARACTER_DATA_PATH,
        );
        return;
    };
    audit.counts.characters = characters.len();
    let attribute_icons = ATTRIBUTE_ICON_PATHS
        .iter()
        .copied()
        .collect::<HashMap<_, _>>();
    for (id, row) in characters {
        let name = json_string(row, "name_zh")
            .or_else(|| json_string(row, "name_en"))
            .unwrap_or_default();
        if name.trim().is_empty() {
            push_item(
                audit,
                ResourceAuditSeverity::Warning,
                ResourceAuditCategory::Character,
                id,
                "未命名角色",
                "缺少中文名",
                CHARACTER_DATA_PATH,
            );
        }
        match json_string(row, "attribute").filter(|value| !value.trim().is_empty()) {
            Some(attribute) => match attribute_icons.get(attribute.as_str()) {
                Some(path) if reader.exists(path) => {}
                Some(path) => push_item(
                    audit,
                    ResourceAuditSeverity::Error,
                    ResourceAuditCategory::Character,
                    id,
                    display_or_id(&name, id),
                    "属性图标缺失",
                    *path,
                ),
                None => push_item(
                    audit,
                    ResourceAuditSeverity::Warning,
                    ResourceAuditCategory::Character,
                    id,
                    display_or_id(&name, id),
                    "属性值未识别",
                    CHARACTER_DATA_PATH,
                ),
            },
            None => push_item(
                audit,
                ResourceAuditSeverity::Warning,
                ResourceAuditCategory::Character,
                id,
                display_or_id(&name, id),
                "缺少属性",
                CHARACTER_DATA_PATH,
            ),
        }
        match json_string(row, "avatar").filter(|value| !value.trim().is_empty()) {
            Some(avatar) if reader.exists(&avatar) => {}
            Some(_) => push_item(
                audit,
                ResourceAuditSeverity::Error,
                ResourceAuditCategory::Character,
                id,
                display_or_id(&name, id),
                "头像资源缺失",
                CHARACTER_DATA_PATH,
            ),
            None => push_item(
                audit,
                ResourceAuditSeverity::Warning,
                ResourceAuditCategory::Character,
                id,
                display_or_id(&name, id),
                "缺少头像路径",
                "res/images/characters",
            ),
        }
    }
}

fn audit_skills(reader: &dyn ResourceReader, audit: &mut ResourceAuditSummary) {
    let mapped_effect_names = load_mapped_effect_names(reader, audit);
    let skill_rows = load_skill_rows(reader, audit);
    let wooden_names = load_wooden_names(reader, audit);
    audit.counts.mapped_effects = mapped_effect_names.len();
    audit.counts.skill_damage = skill_rows.len();
    audit.counts.wooden_names = wooden_names.len();
    for (effect_name, row) in skill_rows {
        if !mapped_effect_names.contains(&effect_name) {
            push_item(
                audit,
                ResourceAuditSeverity::Warning,
                ResourceAuditCategory::GameplayEffect,
                &effect_name,
                &effect_name,
                "技能表存在但 GE index 映射缺失",
                GAMEPLAY_EFFECT_MAPPING_PATH,
            );
        }
        if !wooden_names.contains(&effect_name) {
            push_item(
                audit,
                ResourceAuditSeverity::Warning,
                ResourceAuditCategory::Skill,
                &effect_name,
                &effect_name,
                "缺少中文伤害名",
                WOODEN_DAMAGE_DESCRIPTIONS_PATH,
            );
        }
        let has_category = json_string(&row, "category")
            .or_else(|| json_string(&row, "DamageSourceCategory"))
            .is_some_and(|value| !value.trim().is_empty());
        let has_ability = json_string(&row, "ability")
            .or_else(|| json_string(&row, "GAName"))
            .is_some_and(|value| !value.trim().is_empty() && value != "None");
        if !has_category && !has_ability {
            push_item(
                audit,
                ResourceAuditSeverity::Warning,
                ResourceAuditCategory::Skill,
                &effect_name,
                &effect_name,
                "缺少技能分类或能力名",
                SKILL_DAMAGE_DATA_PATH,
            );
        }
    }
}

fn audit_abyss_monsters(reader: &dyn ResourceReader, audit: &mut ResourceAuditSummary) {
    let Some(document) = read_json_object(reader, ABYSS_MONSTERS_PATH, audit) else {
        return;
    };
    let mut monsters = HashMap::<String, String>::new();
    for season in document
        .get("seasons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for floor in season
            .get("floors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for monster in floor
                .get("monsters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(monster_id) = json_string(monster, "monster_id") else {
                    continue;
                };
                let name = json_string(monster, "name").unwrap_or_default();
                monsters.entry(monster_id).or_insert(name);
            }
        }
    }
    audit.counts.abyss_monsters = monsters.len();
    for (monster_id, name) in monsters {
        if monster_image_candidates(&monster_id)
            .iter()
            .any(|stem| reader.exists(&format!("{MONSTER_IMAGE_DIR}/{stem}.png")))
        {
            continue;
        }
        push_item(
            audit,
            ResourceAuditSeverity::Warning,
            ResourceAuditCategory::Abyss,
            &monster_id,
            display_or_id(&name, &monster_id),
            "深渊怪物头像缺失",
            MONSTER_IMAGE_DIR,
        );
    }
}

fn audit_reactions(reader: &dyn ResourceReader, audit: &mut ResourceAuditSummary) {
    let Some(document) = read_json_object(reader, REACTIONS_PATH, audit) else {
        return;
    };
    let reaction_ids = document
        .get("reactions")
        .and_then(Value::as_object)
        .map(|reactions| {
            reactions
                .keys()
                .filter_map(|key| key.parse::<u8>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    audit.counts.reactions = reaction_ids.len();
    for reaction in 1..=REACTION_TEXT_IMAGE_COUNT {
        if !reaction_ids.contains(&reaction) {
            push_item(
                audit,
                ResourceAuditSeverity::Warning,
                ResourceAuditCategory::Reaction,
                reaction.to_string(),
                "未配置反应",
                "反应表缺少该 ID",
                REACTIONS_PATH,
            );
            continue;
        }
        for part in 1..=2 {
            let path = format!("{DAMAGE_DIGIT_IMAGE_DIR}/fanying{reaction:02}_{part:02}.png");
            if !reader.exists(&path) {
                push_item(
                    audit,
                    ResourceAuditSeverity::Warning,
                    ResourceAuditCategory::Reaction,
                    reaction.to_string(),
                    "反应文字",
                    "反应文字素材缺失",
                    &path,
                );
            }
        }
    }
}

fn load_mapped_effect_names(
    reader: &dyn ResourceReader,
    audit: &mut ResourceAuditSummary,
) -> HashSet<String> {
    let Some(document) = read_json_object(reader, GAMEPLAY_EFFECT_MAPPING_PATH, audit) else {
        return HashSet::new();
    };
    if let Some(effects) = document.get("effects").and_then(Value::as_object) {
        return effects
            .values()
            .filter_map(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .collect();
    }
    data_table_rows(&document)
        .map(|rows| {
            rows.iter()
                .filter_map(|(name, row)| {
                    row.get("UniqueIndex")
                        .and_then(Value::as_u64)
                        .filter(|index| *index != 0)
                        .map(|_| name.clone())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn load_skill_rows(
    reader: &dyn ResourceReader,
    audit: &mut ResourceAuditSummary,
) -> HashMap<String, Value> {
    let Some(document) = read_json_object(reader, SKILL_DAMAGE_DATA_PATH, audit) else {
        return HashMap::new();
    };
    if let Some(skills) = document.get("skills").and_then(Value::as_object) {
        return skills
            .iter()
            .map(|(name, row)| (name.clone(), row.clone()))
            .collect();
    }
    data_table_rows(&document)
        .map(|rows| {
            rows.iter()
                .map(|(name, row)| (name.clone(), row.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn load_wooden_names(
    reader: &dyn ResourceReader,
    audit: &mut ResourceAuditSummary,
) -> HashSet<String> {
    let Some(document) = read_json_object(reader, WOODEN_DAMAGE_DESCRIPTIONS_PATH, audit) else {
        return HashSet::new();
    };
    if let Some(names) = document.get("names").and_then(Value::as_object) {
        return names
            .iter()
            .filter_map(|(name, value)| {
                value
                    .as_str()
                    .filter(|label| !label.trim().is_empty())
                    .map(|_| name.clone())
            })
            .collect();
    }
    data_table_rows(&document)
        .map(|rows| {
            rows.iter()
                .filter_map(|(name, row)| {
                    row.get("Desc")
                        .and_then(|desc| desc.get("CultureInvariantString"))
                        .and_then(Value::as_str)
                        .filter(|description| !description.trim().is_empty())
                        .map(|_| name.clone())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn data_table_rows(document: &Value) -> Option<&serde_json::Map<String, Value>> {
    document
        .as_array()
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("Rows"))
        .and_then(Value::as_object)
}

fn read_json_object(
    reader: &dyn ResourceReader,
    relative_path: &str,
    audit: &mut ResourceAuditSummary,
) -> Option<Value> {
    let text = match reader.read_text(relative_path) {
        Ok(text) => text,
        Err(error) => {
            push_item(
                audit,
                ResourceAuditSeverity::Error,
                ResourceAuditCategory::File,
                relative_path,
                relative_path,
                format!("资源读取失败：{error}"),
                relative_path,
            );
            return None;
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            push_item(
                audit,
                ResourceAuditSeverity::Error,
                ResourceAuditCategory::File,
                relative_path,
                relative_path,
                format!("JSON 无效：{error}"),
                relative_path,
            );
            None
        }
    }
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn display_or_id<'a>(display: &'a str, id: &'a str) -> &'a str {
    if display.trim().is_empty() {
        id
    } else {
        display
    }
}

fn push_item(
    audit: &mut ResourceAuditSummary,
    severity: ResourceAuditSeverity,
    category: ResourceAuditCategory,
    resource_id: impl Into<String>,
    display_name: impl Into<String>,
    message: impl Into<String>,
    suggested_source: impl Into<String>,
) {
    audit.items.push(ResourceAuditItem {
        severity,
        category,
        resource_id: resource_id.into(),
        display_name: display_name.into(),
        message: message.into(),
        suggested_source: suggested_source.into(),
    });
}

fn monster_image_candidates(value: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let raw = value
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(value);
    push_unique(&mut candidates, raw.to_owned());
    push_trimmed_monster_stems(&mut candidates, raw);
    let canonical = canonical_monster_image_key(value);
    push_unique(&mut candidates, canonical.clone());
    push_unique(&mut candidates, titlecase_boss_key(&canonical));
    push_trimmed_monster_keys(&mut candidates, &canonical);
    candidates
}

fn push_trimmed_monster_stems(candidates: &mut Vec<String>, stem: &str) {
    let suffixes = ["_Abyss", "_abyss", "_BP", "_bp", "_BF", "_bf", "_B", "_b"];
    let mut current = stem.to_owned();
    while let Some(next) = suffixes
        .iter()
        .find_map(|suffix| current.strip_suffix(suffix).map(str::to_owned))
    {
        push_unique(candidates, next.clone());
        current = next;
    }
}

fn push_trimmed_monster_keys(candidates: &mut Vec<String>, key: &str) {
    let suffixes = ["_abyss", "_bp", "_bf", "_b"];
    let mut current = key.to_owned();
    while let Some(next) = suffixes
        .iter()
        .find_map(|suffix| current.strip_suffix(suffix).map(str::to_owned))
    {
        push_unique(candidates, next.clone());
        current = next;
    }
    if let Some(without_blue) = current.strip_suffix("_blue") {
        push_unique(candidates, without_blue.to_owned());
    }
    if let Some(without_red) = current.strip_suffix("_red") {
        push_unique(candidates, without_red.to_owned());
    }
    if let Some((base, _)) = current.split_once("_summon") {
        push_unique(candidates, base.to_owned());
    }
    if let Some((base, _)) = current.split_once("_double_") {
        push_unique(candidates, base.to_owned());
    }
}

fn titlecase_boss_key(key: &str) -> String {
    key.strip_prefix("boss_")
        .map(|suffix| format!("Boss_{suffix}"))
        .unwrap_or_else(|| key.to_owned())
}

fn canonical_monster_image_key(value: &str) -> String {
    let without_extension = value
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(value)
        .to_ascii_lowercase();
    without_extension
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u32>()
                .map(|number| number.to_string())
                .unwrap_or_else(|_| part.to_owned())
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audits_missing_runtime_resources_without_external_paths() {
        let root = temp_root("missing-runtime");
        write(
            &root,
            CHARACTER_DATA_PATH,
            r#"{"characters":{"1001":{"name_zh":"测试","attribute":"灵","avatar":"res/images/characters/missing.png"}}}"#,
        );
        write(
            &root,
            GAMEPLAY_EFFECT_MAPPING_PATH,
            r#"{"effects":{"1":"GE_Known"}}"#,
        );
        write(
            &root,
            SKILL_DAMAGE_DATA_PATH,
            r#"{"skills":{"GE_MissingName":{"category":"A"},"GE_Empty":{}}}"#,
        );
        write(
            &root,
            WOODEN_DAMAGE_DESCRIPTIONS_PATH,
            r#"{"names":{"GE_Known":"已知"}}"#,
        );
        write(
            &root,
            ABYSS_MONSTERS_PATH,
            r#"{"seasons":[{"floors":[{"monsters":[{"monster_id":"mon_01_BP","name":"测试怪"}]}]}]}"#,
        );
        write(
            &root,
            REACTIONS_PATH,
            r#"{"reactions":{"1":{"name_zh":"延滞"}}}"#,
        );
        write(&root, "res/images/attributes/UI_avatarbg_Icon_01.png", "");

        let summary = audit_resource_root(&root);

        assert_eq!(summary.counts.characters, 1);
        assert!(summary.items.iter().any(|item| {
            item.category == ResourceAuditCategory::Character && item.message == "头像资源缺失"
        }));
        assert!(summary.items.iter().any(|item| {
            item.category == ResourceAuditCategory::Skill && item.message == "缺少中文伤害名"
        }));
        assert!(summary.items.iter().any(|item| {
            item.category == ResourceAuditCategory::GameplayEffect
                && item.message == "技能表存在但 GE index 映射缺失"
        }));
        assert!(summary.items.iter().any(|item| {
            item.category == ResourceAuditCategory::Abyss && item.message == "深渊怪物头像缺失"
        }));
        assert!(summary.items.iter().any(|item| {
            item.category == ResourceAuditCategory::Reaction && item.message == "反应文字素材缺失"
        }));
        assert!(
            !summary
                .redacted_text()
                .contains(&root.display().to_string())
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn redacted_report_does_not_copy_missing_avatar_paths() {
        let root = temp_root("avatar-redaction");
        let private_avatar = r#"C:\Users\KongBai\Pictures\custom_avatar.png"#;
        write(
            &root,
            CHARACTER_DATA_PATH,
            &format!(
                r#"{{"characters":{{"1001":{{"name_zh":"测试","attribute":"灵","avatar":{}}}}}}}"#,
                serde_json::to_string(private_avatar).unwrap()
            ),
        );
        write(&root, "res/images/attributes/UI_avatarbg_Icon_01.png", "");

        let summary = audit_resource_root(&root);
        let avatar_item = summary
            .items
            .iter()
            .find(|item| {
                item.category == ResourceAuditCategory::Character && item.message == "头像资源缺失"
            })
            .expect("missing avatar item");
        let report = summary.redacted_text();

        assert_eq!(avatar_item.suggested_source, CHARACTER_DATA_PATH);
        assert!(!report.contains(private_avatar));
        assert!(!report.contains("KongBai"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn audits_legacy_skill_resource_tables() {
        let root = temp_root("legacy-skill-tables");
        write(
            &root,
            GAMEPLAY_EFFECT_MAPPING_PATH,
            r#"[{"Rows":{"GE_Known":{"UniqueIndex":1001}}}]"#,
        );
        write(
            &root,
            SKILL_DAMAGE_DATA_PATH,
            r#"[{"Rows":{"GE_Known":{"DamageSourceCategory":"EDamageSourceCategory::Normal","GAName":"GA_Test"},"GE_Missing":{"DamageSourceCategory":"EDamageSourceCategory::Normal"}}}]"#,
        );
        write(
            &root,
            WOODEN_DAMAGE_DESCRIPTIONS_PATH,
            r#"[{"Rows":{"GE_Known":{"Desc":{"CultureInvariantString":"普攻1段"}}}}]"#,
        );

        let summary = audit_resource_root(&root);

        assert_eq!(summary.counts.skill_damage, 2);
        assert_eq!(summary.counts.mapped_effects, 1);
        assert!(summary.items.iter().any(|item| {
            item.category == ResourceAuditCategory::Skill
                && item.resource_id == "GE_Missing"
                && item.message == "缺少中文伤害名"
        }));
        assert!(summary.items.iter().any(|item| {
            item.category == ResourceAuditCategory::GameplayEffect
                && item.resource_id == "GE_Missing"
                && item.message == "技能表存在但 GE index 映射缺失"
        }));

        let _ = std::fs::remove_dir_all(root);
    }

    fn write(root: &Path, relative_path: &str, text: &str) {
        let path = root.join(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nte_resource_audit_test_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
