use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

include!(concat!(env!("OUT_DIR"), "/embedded_resources.rs"));

pub(crate) fn bundled_resource(path: &str) -> Option<&'static [u8]> {
    embedded_resource(path)
}

pub(crate) fn resource_file_path(path: &Path) -> Option<PathBuf> {
    disk_resource_candidates(path)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

pub(crate) fn resource_exists(path: &Path) -> bool {
    resource_file_path(path).is_some() || bundled_resource_for_path(path).is_some()
}

pub(crate) fn read_resource_text(path: &Path) -> Result<String> {
    let bytes = read_resource_bytes(path)?;
    String::from_utf8(bytes.into_owned())
        .with_context(|| format!("资源不是 UTF-8 文本 {}", path.display()))
}

pub(crate) fn read_resource_bytes(path: &Path) -> Result<Cow<'static, [u8]>> {
    if let Some(disk_path) = resource_file_path(path) {
        let bytes = std::fs::read(&disk_path)
            .with_context(|| format!("无法读取资源 {}", disk_path.display()))?;
        return Ok(Cow::Owned(bytes));
    }

    if let Some(bytes) = bundled_resource_for_path(path) {
        return Ok(Cow::Borrowed(bytes));
    }

    Err(anyhow!("找不到资源 {}", path.display()))
}

fn bundled_resource_for_path(path: &Path) -> Option<&'static [u8]> {
    let key = embedded_resource_key(path)?;
    bundled_resource(&key)
}

fn disk_resource_candidates(path: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    push_unique_path(&mut candidates, path.to_path_buf());

    if !path.is_absolute() {
        if let Ok(current_dir) = std::env::current_dir() {
            push_unique_path(&mut candidates, current_dir.join(path));
        }

        if let Ok(executable) = std::env::current_exe() {
            for ancestor in executable.ancestors().skip(1) {
                push_unique_path(&mut candidates, ancestor.join(path));
            }
        }

        push_unique_path(
            &mut candidates,
            Path::new(env!("CARGO_MANIFEST_DIR")).join(path),
        );
    }

    candidates
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn embedded_resource_key(path: &Path) -> Option<String> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    if normalized == "res" || normalized.starts_with("res/") {
        return Some(normalized.to_owned());
    }
    normalized
        .find("/res/")
        .map(|index| normalized[index + 1..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn bundled_resource_contains_character_data() {
        let bytes = bundled_resource("res/data/characters/characters.json")
            .expect("characters.json should be bundled");

        assert!(std::str::from_utf8(bytes).unwrap().contains("characters"));
    }

    #[test]
    fn missing_res_path_falls_back_to_bundled_resource() {
        let path = Path::new("missing-root/res/data/characters/characters.json");
        let text = read_resource_text(path).expect("bundled characters should load");

        assert!(text.contains("characters"));
    }

    #[test]
    fn disk_file_wins_over_matching_bundled_resource_key() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("nte-resource-test-{}-{unique}", std::process::id()));
        let path = root.join("res/data/characters/characters.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "disk wins").unwrap();

        let text = read_resource_text(&path).expect("disk resource should load");

        assert_eq!(text, "disk wins");
        let _ = std::fs::remove_dir_all(root);
    }
}
