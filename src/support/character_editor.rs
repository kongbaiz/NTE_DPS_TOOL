use std::path::Path;

use crate::storage::resource::read_resource_text;

pub(crate) const CHARACTER_ATTRIBUTES: [&str; 6] = ["灵", "咒", "光", "魂", "暗", "相"];

#[derive(Clone, Default)]
pub(crate) struct CharacterEditForm {
    pub(crate) id: String,
    pub(crate) name_zh: String,
    pub(crate) name_en: String,
    pub(crate) codename: String,
    pub(crate) attribute: String,
    pub(crate) verified: bool,
    pub(crate) color: String,
    pub(crate) avatar: String,
}

pub(crate) struct CharacterEditorState {
    pub(crate) document: serde_json::Value,
    pub(crate) selected_id: Option<String>,
    pub(crate) form: CharacterEditForm,
    pub(crate) search: String,
    pub(crate) new_id: String,
    pub(crate) dirty: bool,
    pub(crate) message: String,
    pub(crate) cancel_selection: Option<String>,
}

impl CharacterEditorState {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let text = read_resource_text(path)
            .map_err(|error| format!("无法读取 {}: {error}", path.display()))?;
        let document: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| format!("角色表 JSON 无效: {error}"))?;
        if !document
            .get("characters")
            .is_some_and(serde_json::Value::is_object)
        {
            return Err("characters.json 缺少 characters 对象".to_owned());
        }
        Ok(Self {
            document,
            selected_id: None,
            form: CharacterEditForm::default(),
            search: String::new(),
            new_id: String::new(),
            dirty: false,
            message: String::new(),
            cancel_selection: None,
        })
    }

    pub(crate) fn character_ids(&self) -> Vec<String> {
        let mut ids = self
            .document
            .get("characters")
            .and_then(serde_json::Value::as_object)
            .map(|characters| characters.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        ids.sort_by_key(|id| id.parse::<u32>().unwrap_or(u32::MAX));
        ids
    }

    pub(crate) fn select(&mut self, id: &str) {
        let Some(row) = self
            .document
            .get("characters")
            .and_then(serde_json::Value::as_object)
            .and_then(|characters| characters.get(id))
            .and_then(serde_json::Value::as_object)
        else {
            return;
        };
        self.selected_id = Some(id.to_owned());
        self.form = CharacterEditForm {
            id: id.to_owned(),
            name_zh: json_string_field(row, "name_zh"),
            name_en: json_string_field(row, "name_en"),
            codename: json_string_field(row, "codename"),
            attribute: json_string_field(row, "attribute"),
            verified: row
                .get("verified")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            color: json_string_field(row, "color"),
            avatar: json_string_field(row, "avatar"),
        };
        self.dirty = false;
        self.message.clear();
        self.cancel_selection = None;
    }

    pub(crate) fn start_new(&mut self) -> Result<(), String> {
        let id = self.new_id.trim();
        let parsed = id
            .parse::<u32>()
            .map_err(|_| "角色 ID 必须是正整数".to_owned())?;
        if parsed == 0 {
            return Err("角色 ID 必须大于 0".to_owned());
        }
        let id = parsed.to_string();
        if self
            .document
            .get("characters")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|characters| characters.contains_key(&id))
        {
            self.select(&id);
            return Err(format!("ID {id} 已存在，已切换到现有记录"));
        }
        self.cancel_selection = self.selected_id.clone();
        self.selected_id = None;
        self.form = CharacterEditForm {
            id,
            ..Default::default()
        };
        self.new_id.clear();
        self.dirty = true;
        self.message = "正在新增角色，填写后保存".to_owned();
        Ok(())
    }

    pub(crate) fn apply_form(&mut self) -> Result<String, String> {
        let id = self
            .form
            .id
            .trim()
            .parse::<u32>()
            .map_err(|_| "角色 ID 必须是正整数".to_owned())?
            .to_string();
        if self.form.name_zh.trim().is_empty() && self.form.name_en.trim().is_empty() {
            return Err("中文名和英文名至少填写一项".to_owned());
        }
        let color = self.form.color.trim();
        if !color.is_empty() && !is_hex_color(color) {
            return Err("颜色必须是 #RRGGBB 格式".to_owned());
        }
        let attribute = self.form.attribute.trim();
        if !attribute.is_empty() && !CHARACTER_ATTRIBUTES.contains(&attribute) {
            return Err(format!(
                "角色属性必须是：{}",
                CHARACTER_ATTRIBUTES.join("、")
            ));
        }
        if let Some(selected_id) = &self.selected_id
            && selected_id != &id
        {
            return Err("现有角色 ID 不允许直接修改，请新增记录".to_owned());
        }
        let characters = self
            .document
            .get_mut("characters")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| "characters.json 缺少 characters 对象".to_owned())?;
        let row = characters
            .entry(id.clone())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let row = row
            .as_object_mut()
            .ok_or_else(|| format!("ID {id} 的数据不是 JSON 对象"))?;
        set_json_string(row, "name_zh", self.form.name_zh.trim());
        set_json_string(row, "name_en", self.form.name_en.trim());
        set_json_string(row, "codename", self.form.codename.trim());
        set_optional_json_string(row, "attribute", attribute);
        row.insert(
            "verified".to_owned(),
            serde_json::Value::Bool(self.form.verified),
        );
        set_optional_json_string(row, "color", color);
        set_optional_json_string(row, "avatar", self.form.avatar.trim());
        self.selected_id = Some(id.clone());
        self.form.id = id.clone();
        self.dirty = false;
        self.cancel_selection = None;
        Ok(id)
    }

    pub(crate) fn cancel_edit(&mut self) {
        if let Some(id) = self
            .cancel_selection
            .take()
            .or_else(|| self.selected_id.clone())
        {
            self.select(&id);
        } else {
            self.form = CharacterEditForm::default();
            self.dirty = false;
            self.message.clear();
        }
    }
}

pub(crate) fn json_string_field(
    row: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> String {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn set_json_string(row: &mut serde_json::Map<String, serde_json::Value>, key: &str, value: &str) {
    row.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
}

fn set_optional_json_string(
    row: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: &str,
) {
    if value.is_empty() {
        row.remove(key);
    } else {
        set_json_string(row, key, value);
    }
}

fn is_hex_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
}
