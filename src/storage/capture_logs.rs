//! Management of the raw capture logs the engine writes to
//! `logs/nte_raw_*.pcapng`. Lets the UI report how much disk the captures use and
//! clear them on demand. The active capture's file is held open by the OS, so a
//! delete attempt on it simply fails and is skipped — it is never force-removed.

use std::fs;
use std::path::Path;

const CAPTURE_LOG_PREFIX: &str = "nte_raw_";
const CAPTURE_LOG_EXTENSION: &str = "pcapng";

/// Directory the capture engine writes raw frames to, relative to the process
/// working directory — the same location `RawCaptureBuffer` uses.
pub const CAPTURE_LOG_DIR: &str = "logs";

/// Count and total on-disk size of the raw capture logs.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct CaptureLogStats {
    pub count: usize,
    pub total_bytes: u64,
}

/// Result of a clear request: how many files were removed, how much was freed,
/// and how many could not be removed (e.g. the locked active capture).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ClearOutcome {
    pub deleted: usize,
    pub freed_bytes: u64,
    pub failed: usize,
}

fn is_capture_log(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with(CAPTURE_LOG_PREFIX)
        && path.extension().and_then(|ext| ext.to_str()) == Some(CAPTURE_LOG_EXTENSION)
}

/// Count and total the size of capture logs in `dir`. A missing or unreadable
/// directory is reported as empty rather than an error.
pub fn scan_capture_logs(dir: &Path) -> CaptureLogStats {
    let mut stats = CaptureLogStats::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return stats;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_capture_log(&path) {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            stats.count += 1;
            stats.total_bytes = stats.total_bytes.saturating_add(meta.len());
        }
    }
    stats
}

/// Delete every capture log in `dir`. Files that can't be removed (most often the
/// active capture's locked file) are counted as failures and left in place.
pub fn clear_capture_logs(dir: &Path) -> ClearOutcome {
    let mut outcome = ClearOutcome::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return outcome;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_capture_log(&path) {
            continue;
        }
        let Some(size) = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len())
        else {
            continue;
        };
        match fs::remove_file(&path) {
            Ok(()) => {
                outcome.deleted += 1;
                outcome.freed_bytes = outcome.freed_bytes.saturating_add(size);
            }
            Err(_) => outcome.failed += 1,
        }
    }
    outcome
}

/// Human-readable size for UI display (e.g. `0 B`, `512 B`, `1.5 KB`, `12.3 MB`).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, name: &str, bytes: usize) {
        fs::write(dir.join(name), vec![0u8; bytes]).unwrap();
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nte_capture_logs_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_directory_scans_as_empty() {
        let stats = scan_capture_logs(Path::new("definitely/missing/dir"));
        assert_eq!(stats, CaptureLogStats::default());
    }

    #[test]
    fn scan_counts_only_capture_logs() {
        let dir = temp_dir("scan");
        write_file(&dir, "nte_raw_20260101_000000_000.pcapng", 100);
        write_file(&dir, "nte_raw_20260102_000000_000.pcapng", 200);
        write_file(&dir, "nte_panic_20260101.log", 999);
        write_file(&dir, "notes.txt", 50);
        write_file(&dir, "nte_raw_export.json", 50);

        let stats = scan_capture_logs(&dir);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_bytes, 300);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clear_removes_only_capture_logs_and_reports_freed() {
        let dir = temp_dir("clear");
        write_file(&dir, "nte_raw_a.pcapng", 100);
        write_file(&dir, "nte_raw_b.pcapng", 250);
        write_file(&dir, "keep.txt", 10);

        let outcome = clear_capture_logs(&dir);
        assert_eq!(outcome.deleted, 2);
        assert_eq!(outcome.freed_bytes, 350);
        assert_eq!(outcome.failed, 0);
        assert!(dir.join("keep.txt").exists());
        assert_eq!(scan_capture_logs(&dir), CaptureLogStats::default());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
