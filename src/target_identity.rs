pub fn canonical_target_key_for_path(path: &str) -> Option<String> {
    let clean = clean_identifier(path);
    if clean.is_empty() {
        return None;
    }
    let lower = clean.to_ascii_lowercase();
    for marker in ["worldboss_boss", "weeklyclone_boss", "boss_", "mon_"] {
        if let Some((prefix, number)) = target_number_after_marker(&lower, marker) {
            return Some(format!("monster:{prefix}_{number:02}"));
        }
    }
    let basename = lower
        .rsplit('/')
        .next()
        .unwrap_or(&lower)
        .rsplit('.')
        .next()
        .unwrap_or(&lower)
        .trim_start_matches("default__")
        .trim_end_matches("_c");
    for marker in ["boss", "mon"] {
        if let Some((prefix, number)) = target_number_after_marker(basename, marker) {
            return Some(format!("monster:{prefix}_{number:02}"));
        }
    }
    None
}

pub fn canonical_target_key_from_name_and_path(name: &str, path: &str) -> Option<String> {
    canonical_target_key_for_path(path).or_else(|| canonical_target_key_for_path(name))
}

pub fn is_boss_target_key(key: &str) -> bool {
    key.trim().to_ascii_lowercase().starts_with("monster:boss_")
}

pub fn is_small_monster_target_key(key: &str) -> bool {
    key.trim().to_ascii_lowercase().starts_with("monster:mon_")
}

fn target_number_after_marker(value: &str, marker: &str) -> Option<(&'static str, u32)> {
    let prefix = if marker.contains("boss") {
        "boss"
    } else if marker.contains("mon") {
        "mon"
    } else {
        return None;
    };
    let mut search_start = 0;
    while let Some(relative_index) = value[search_start..].find(marker) {
        let digit_start = search_start + relative_index + marker.len();
        let digits = value[digit_start..]
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        if !digits.is_empty() {
            return digits.parse::<u32>().ok().map(|number| (prefix, number));
        }
        search_start = digit_start.saturating_add(1);
        if search_start >= value.len() {
            break;
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worldboss_alias_and_ue_class_path_same_canonical_key() {
        let worldboss = canonical_target_key_for_path("WorldBoss_Boss13");
        let class_path = canonical_target_key_for_path(
            "/Game/Blueprints/Character/Monster/boss_13/boss_13_BP.boss_13_BP_C",
        );
        let instance_path = canonical_target_key_for_path(
            "/Game/Blueprints/Character/Monster/boss_13/boss_13_BP.boss_13_BP_C_2147482486",
        );
        assert_eq!(worldboss.as_deref(), Some("monster:boss_13"));
        assert_eq!(worldboss, class_path);
        assert_eq!(worldboss, instance_path);
        assert_eq!(
            canonical_target_key_for_path("WorldBoss_Boss08").as_deref(),
            Some("monster:boss_08")
        );
        assert_eq!(
            canonical_target_key_for_path("boss_08_BP").as_deref(),
            Some("monster:boss_08")
        );
        assert!(is_boss_target_key("monster:boss_08"));
        assert!(!is_small_monster_target_key("monster:boss_08"));
        assert!(is_small_monster_target_key("monster:mon_01"));
        assert!(!is_boss_target_key("monster:mon_01"));
    }
}
