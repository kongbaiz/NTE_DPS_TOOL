use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::target_instance::{TargetAlias, TargetAliasKind};

const RELATIVE_TIME_CUTOFF_SECONDS: f64 = 10_000_000.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeMappingAction {
    Map,
    Close,
    Destroy,
}

#[derive(Clone, Debug)]
pub struct RuntimeMappingEvent {
    pub timestamp: f64,
    pub action: RuntimeMappingAction,
    pub object_path: Option<String>,
    pub class_path: Option<String>,
    pub target_name: Option<String>,
    pub aliases: Vec<TargetAlias>,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeMappingTimeline {
    events: VecDeque<RuntimeMappingEvent>,
    aligned: bool,
}

impl RuntimeMappingTimeline {
    pub fn new(mut events: Vec<RuntimeMappingEvent>) -> Self {
        events.sort_by(|left, right| left.timestamp.total_cmp(&right.timestamp));
        Self {
            events: events.into(),
            aligned: false,
        }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn align_to_packet_time(&mut self, packet_timestamp: f64) {
        if self.aligned {
            return;
        }
        self.aligned = true;
        let Some(first) = self.events.front() else {
            return;
        };
        if first.timestamp >= RELATIVE_TIME_CUTOFF_SECONDS
            || packet_timestamp < RELATIVE_TIME_CUTOFF_SECONDS
        {
            return;
        }
        let offset = packet_timestamp - first.timestamp;
        for event in &mut self.events {
            event.timestamp += offset;
        }
    }

    pub fn pop_due(&mut self, timestamp: f64) -> Vec<RuntimeMappingEvent> {
        let mut due = Vec::new();
        while self
            .events
            .front()
            .is_some_and(|event| event.timestamp <= timestamp)
        {
            if let Some(event) = self.events.pop_front() {
                due.push(event);
            }
        }
        due
    }

    pub fn pop_all(&mut self) -> Vec<RuntimeMappingEvent> {
        self.events.drain(..).collect()
    }
}

pub fn find_companion_runtime_mapping_sidecar(capture_path: &Path) -> Option<PathBuf> {
    runtime_mapping_sidecar_candidates(capture_path)
        .into_iter()
        .find(|path| path.is_file())
}

pub fn load_runtime_mapping_sidecar(path: &Path) -> Result<RuntimeMappingTimeline> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("runtime mapping sidecar read failed: {}", path.display()))?;
    let events = parse_runtime_mapping_events(&text)
        .with_context(|| format!("runtime mapping sidecar parse failed: {}", path.display()))?;
    Ok(RuntimeMappingTimeline::new(events))
}

fn parse_runtime_mapping_events(text: &str) -> Result<Vec<RuntimeMappingEvent>> {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => events_from_json_value(&value),
        Err(json_error) => {
            let mut events = Vec::new();
            for (line_index, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let value = serde_json::from_str::<Value>(line).with_context(|| {
                    format!(
                        "invalid JSON and JSONL line {} is not valid JSON: {json_error}",
                        line_index + 1
                    )
                })?;
                events.push(event_from_value(&value).with_context(|| {
                    format!("runtime mapping line {} is not an event", line_index + 1)
                })?);
            }
            Ok(events)
        }
    }
}

fn events_from_json_value(value: &Value) -> Result<Vec<RuntimeMappingEvent>> {
    if let Some(events) = value
        .get("events")
        .or_else(|| value.get("Events"))
        .and_then(Value::as_array)
    {
        return events.iter().map(event_from_value).collect();
    }
    if let Some(events) = value.as_array() {
        return events.iter().map(event_from_value).collect();
    }
    Ok(vec![event_from_value(value)?])
}

fn event_from_value(value: &Value) -> Result<RuntimeMappingEvent> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("event must be a JSON object"))?;
    let timestamp = number_field(value, &["timestamp", "time", "ts", "seconds"])
        .ok_or_else(|| anyhow!("event is missing timestamp/time/ts"))?;
    let action = action_from_event_name(string_field(value, &["event", "type", "action"]));
    let connection = string_field(value, &["connection", "conn", "driver", "net_driver"]);
    let aliases = aliases_from_value(value, connection.as_deref());
    let object_path = string_field(
        value,
        &["object_path", "actor_path", "object", "actor", "path"],
    );
    let class_path = string_field(value, &["class_path", "class", "actor_class"]);
    let target_name = string_field(value, &["target_name", "display_name", "name"]);
    if aliases.is_empty() && object_path.is_none() && class_path.is_none() {
        return Err(anyhow!(
            "event must include at least one alias or object/class path: {object:?}"
        ));
    }
    Ok(RuntimeMappingEvent {
        timestamp,
        action,
        object_path,
        class_path,
        target_name,
        aliases,
    })
}

fn aliases_from_value(value: &Value, connection: Option<&str>) -> Vec<TargetAlias> {
    let mut aliases = Vec::new();
    if let Some(channel) = scalar_field(value, &["actor_channel", "channel", "channel_id"]) {
        let channel = match connection {
            Some(connection) if !connection.trim().is_empty() => {
                format!("{}:{}", connection.trim(), channel)
            }
            _ => channel,
        };
        aliases.push(TargetAlias::new(TargetAliasKind::ActorChannel, channel));
    }
    push_alias_fields(
        value,
        &mut aliases,
        TargetAliasKind::IrisRef32,
        &[
            "iris_ref32",
            "iris_ref",
            "iris",
            "netref",
            "net_ref_handle",
            "netref_handle",
        ],
    );
    push_alias_fields(
        value,
        &mut aliases,
        TargetAliasKind::NetGuid32,
        &["netguid32", "net_guid32", "netguid", "net_guid"],
    );
    push_alias_fields(
        value,
        &mut aliases,
        TargetAliasKind::NetGuidPacked,
        &["netguid_packed", "net_guid_packed"],
    );
    push_alias_fields(
        value,
        &mut aliases,
        TargetAliasKind::SdkNetTarget,
        &["sdk_net_target", "net_target", "fcharacter_for_net"],
    );
    push_alias_fields(
        value,
        &mut aliases,
        TargetAliasKind::BossHpGuid,
        &["boss_hp_guid", "attribute_guid", "hp_guid"],
    );
    push_alias_fields(
        value,
        &mut aliases,
        TargetAliasKind::CurrentHpToken,
        &["current_hp_token", "currenthp_token"],
    );
    aliases.sort_by(|left, right| left.key().cmp(&right.key()));
    aliases.dedup_by(|left, right| left.key() == right.key());
    aliases
}

fn push_alias_fields(
    value: &Value,
    aliases: &mut Vec<TargetAlias>,
    kind: TargetAliasKind,
    fields: &[&str],
) {
    for field in fields {
        if let Some(value) = scalar_field(value, &[*field]) {
            aliases.push(TargetAlias::new(
                kind,
                normalize_handle_for_kind(kind, value),
            ));
        }
    }
}

fn normalize_handle_for_kind(kind: TargetAliasKind, value: String) -> String {
    let value = value.trim();
    if value.starts_with("0x") || value.starts_with("0X") {
        return format!("0x{}", value[2..].to_ascii_lowercase());
    }
    if matches!(
        kind,
        TargetAliasKind::IrisRef32 | TargetAliasKind::NetGuid32 | TargetAliasKind::NetGuidPacked
    ) && value.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(number) = value.parse::<u32>()
    {
        return match kind {
            TargetAliasKind::NetGuidPacked => format!("0x{number:x}"),
            _ => format!("0x{number:08x}"),
        };
    }
    value.to_owned()
}

fn action_from_event_name(value: Option<String>) -> RuntimeMappingAction {
    let Some(value) = value else {
        return RuntimeMappingAction::Map;
    };
    let lower = value.to_ascii_lowercase();
    if lower.contains("destroy") || lower.contains("delete") || lower.contains("remove_object") {
        RuntimeMappingAction::Destroy
    } else if lower.contains("close") || lower.contains("remove") || lower.contains("unmap") {
        RuntimeMappingAction::Close
    } else {
        RuntimeMappingAction::Map
    }
}

fn string_field(value: &Value, fields: &[&str]) -> Option<String> {
    scalar_field(value, fields).filter(|value| !value.trim().is_empty() && value != "None")
}

fn scalar_field(value: &Value, fields: &[&str]) -> Option<String> {
    for field in fields {
        let Some(value) = value.get(*field) else {
            continue;
        };
        match value {
            Value::String(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    return Some(text.to_owned());
                }
            }
            Value::Number(number) => {
                return Some(number.to_string());
            }
            _ => {}
        }
    }
    None
}

fn number_field(value: &Value, fields: &[&str]) -> Option<f64> {
    for field in fields {
        let Some(value) = value.get(*field) else {
            continue;
        };
        match value {
            Value::Number(number) => return number.as_f64(),
            Value::String(text) => {
                if let Ok(number) = text.trim().parse::<f64>() {
                    return Some(number);
                }
            }
            _ => {}
        }
    }
    None
}

fn runtime_mapping_sidecar_candidates(capture_path: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(stem) = capture_path.file_stem().and_then(|value| value.to_str()) {
        let parent = capture_path.parent().unwrap_or_else(|| Path::new(""));
        for suffix in [
            ".sidecar.jsonl",
            ".sidecar.json",
            ".runtime.jsonl",
            ".runtime.json",
            ".mapping.jsonl",
            ".mapping.json",
        ] {
            candidates.push(parent.join(format!("{stem}{suffix}")));
        }
    }
    if let Some(name) = capture_path.file_name().and_then(|value| value.to_str()) {
        let parent = capture_path.parent().unwrap_or_else(|| Path::new(""));
        for suffix in [".sidecar.jsonl", ".sidecar.json"] {
            candidates.push(parent.join(format!("{name}{suffix}")));
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonl_runtime_mapping_events() {
        let text = r#"{"time":1.0,"event":"actor_channel_open","connection":"GameNetDriver","channel":42,"netguid32":"0x40A54CD0","iris_ref32":748166873,"path":"WorldBoss_Boss33","target_name":"测试目标"}
{"time":2.0,"event":"actor_channel_close","connection":"GameNetDriver","channel":42}"#;

        let events = parse_runtime_mapping_events(text).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].action, RuntimeMappingAction::Map);
        assert_eq!(events[1].action, RuntimeMappingAction::Close);
        assert!(
            events[0]
                .aliases
                .iter()
                .any(|alias| alias.key() == "netguid32:0x40a54cd0")
        );
        assert!(
            events[0]
                .aliases
                .iter()
                .any(|alias| alias.key() == "iris_ref32:0x2c981ed9")
        );
        assert!(
            events[0]
                .aliases
                .iter()
                .any(|alias| alias.key() == "actor_channel:gamenetdriver:42")
        );
    }

    #[test]
    fn aligns_relative_sidecar_time_to_first_packet_time() {
        let event = RuntimeMappingEvent {
            timestamp: 1.0,
            action: RuntimeMappingAction::Map,
            object_path: Some("mon_01_BP".to_owned()),
            class_path: None,
            target_name: Some("低语种".to_owned()),
            aliases: Vec::new(),
        };
        let mut timeline = RuntimeMappingTimeline::new(vec![event]);

        timeline.align_to_packet_time(1_718_000_000.0);
        assert!(timeline.pop_due(1_718_000_000.0).len() == 1);
    }
}
