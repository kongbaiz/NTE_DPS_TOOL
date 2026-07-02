use crate::engine::capture::list_devices;
use crate::platform::network::detect_game_device;
use crate::storage::i18n::{t, tf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiagnosticStatus {
    Passed,
    #[default]
    Warning,
    Failed,
}

impl DiagnosticStatus {
    /// Localized status word for the active UI language.
    pub fn label(self) -> String {
        match self {
            Self::Passed => t("Passed"),
            Self::Warning => t("Warning"),
            Self::Failed => t("Failed"),
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Failed => 0,
            Self::Warning => 1,
            Self::Passed => 2,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticCheck {
    pub status: DiagnosticStatus,
    pub title: String,
    pub detail: String,
    pub suggestion: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticReport {
    pub checks: Vec<DiagnosticCheck>,
}

impl DiagnosticReport {
    pub fn failed_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == DiagnosticStatus::Failed)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == DiagnosticStatus::Warning)
            .count()
    }

    pub fn redacted_text(&self) -> String {
        let mut text = String::new();
        text.push_str(&t("NTE DPS TOOL auto-diagnostics report"));
        text.push('\n');
        text.push_str(&tf(
            "Failed {}, warnings {}",
            &[
                &self.failed_count().to_string(),
                &self.warning_count().to_string(),
            ],
        ));
        text.push('\n');
        for check in &self.checks {
            text.push_str(&format!(
                "[{}] {} - {}\n",
                check.status.label(),
                check.title,
                check.suggestion
            ));
        }
        text.trim_end().to_owned()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DiagnosticSnapshot {
    pub capture_running: bool,
    pub replay_running: bool,
    pub active_capture_filter: Option<String>,
    pub raw_packet_count: usize,
    pub parsed_packet_count: usize,
    pub hit_count: usize,
    pub include_incoming: bool,
    pub server_damage_calibration: bool,
    pub last_diagnostic: Option<String>,
}

pub fn run_capture_diagnostics(snapshot: DiagnosticSnapshot) -> DiagnosticReport {
    let mut checks = Vec::new();
    match list_devices() {
        Ok(devices) if devices.is_empty() => {
            checks.push(check(
                DiagnosticStatus::Failed,
                t("Npcap Device"),
                t("Npcap is loaded but returned no usable capture devices"),
                t("Confirm Npcap is fully installed, and try running as administrator"),
            ));
            checks.push(check(
                DiagnosticStatus::Failed,
                t("Game Connection"),
                t("No matching capture device, cannot locate the game connection"),
                t("Fix Npcap device enumeration first, then enter a game scene and re-run diagnostics"),
            ));
        }
        Ok(devices) => {
            checks.push(check(
                DiagnosticStatus::Passed,
                t("Npcap Device"),
                tf("Detected {} usable devices", &[&devices.len().to_string()]),
                t("Device enumeration is working"),
            ));
            match detect_game_device(&devices) {
                Ok((_, network)) => checks.push(check(
                    DiagnosticStatus::Passed,
                    t("Game Connection"),
                    tf("Located HTGame.exe PID {}", &[&network.pid.to_string()]),
                    t("Detected an active HTGame.exe connection and matching NIC"),
                )),
                Err(error) => checks.push(check(
                    DiagnosticStatus::Failed,
                    t("Game Connection"),
                    error,
                    t("No active HTGame.exe connection detected; enter a game scene before starting capture"),
                )),
            }
        }
        Err(error) => {
            checks.push(check(
                DiagnosticStatus::Failed,
                t("Npcap Device"),
                error,
                t("Install Npcap and confirm WinPcap API-compatible Mode is available"),
            ));
            checks.push(check(
                DiagnosticStatus::Failed,
                t("Game Connection"),
                t("Npcap unavailable, skipping game-connection lookup"),
                t("Fix the Npcap loading issue first, then re-run diagnostics"),
            ));
        }
    }

    if snapshot.capture_running {
        checks.push(check(
            DiagnosticStatus::Passed,
            t("Capture Status"),
            snapshot.active_capture_filter.as_deref().map_or_else(
                || t("Live capture started, BPF is being determined"),
                |filter| tf("Live capture started, BPF={}", &[filter]),
            ),
            t("A live capture task is running"),
        ));
    } else if snapshot.replay_running {
        checks.push(check(
            DiagnosticStatus::Passed,
            t("Capture Status"),
            t("Importing a replay"),
            t("Replay import in progress; live-capture checks do not apply"),
        ));
    } else {
        checks.push(check(
            DiagnosticStatus::Warning,
            t("Capture Status"),
            t("No live capture task right now"),
            t("Run diagnostics after clicking Start to see BPF and raw-capture write status"),
        ));
    }

    if snapshot.raw_packet_count > 0 {
        checks.push(check(
            DiagnosticStatus::Passed,
            t("Raw Capture"),
            tf(
                "Wrote {} raw packets",
                &[&snapshot.raw_packet_count.to_string()],
            ),
            t("Raw PCAPNG writing is working"),
        ));
    } else if snapshot.capture_running {
        checks.push(check(
            DiagnosticStatus::Warning,
            t("Raw Capture"),
            t("Capture is running but no raw packets written yet"),
            t("Confirm the game is in an online scene; narrow or reset the BPF if needed"),
        ));
    } else {
        checks.push(check(
            DiagnosticStatus::Warning,
            t("Raw Capture"),
            t("No raw packets available right now"),
            t("Start capture and wait for the game to produce network traffic, then re-check"),
        ));
    }

    if snapshot.hit_count > 0 {
        checks.push(check(
            DiagnosticStatus::Passed,
            t("Damage Parsing"),
            tf(
                "Parsed {} damage records",
                &[&snapshot.hit_count.to_string()],
            ),
            t("Damage parsing already has results"),
        ));
    } else if snapshot.parsed_packet_count > 0 || snapshot.raw_packet_count > 0 {
        checks.push(check(
            DiagnosticStatus::Warning,
            t("Damage Parsing"),
            tf(
                "{} parsed packets but no damage yet",
                &[&snapshot.parsed_packet_count.to_string()],
            ),
            t("Enter combat and deal damage; if still 0, import a PCAPNG to the diagnostics page to review"),
        ));
    } else {
        checks.push(check(
            DiagnosticStatus::Warning,
            t("Damage Parsing"),
            t("No packet or damage data yet"),
            t("Enter a game scene and start capture first, and confirm the status bar no longer reports capture errors"),
        ));
    }

    checks.push(check(
        if snapshot.include_incoming {
            DiagnosticStatus::Passed
        } else {
            DiagnosticStatus::Warning
        },
        t("Incoming Records"),
        if snapshot.include_incoming {
            t("Incoming parsing is enabled")
        } else {
            t("Incoming parsing is disabled")
        },
        if snapshot.include_incoming {
            t("Incoming stats will be included in the parse-quality report")
        } else {
            t("To investigate direction detection, enable incoming records and re-capture")
        },
    ));

    checks.push(check(
        DiagnosticStatus::Passed,
        t("Server Calibration"),
        if snapshot.server_damage_calibration {
            t("Server-side HP delta calibration is enabled")
        } else {
            t("Server-side HP delta calibration is disabled")
        },
        if snapshot.server_damage_calibration {
            t("Damage values use server-side HP deltas when they can be unambiguously paired")
        } else {
            t("To investigate damage-value deviation, enable calibration and re-capture or re-import")
        },
    ));

    if let Some(diagnostic) = snapshot
        .last_diagnostic
        .filter(|diagnostic| !diagnostic.trim().is_empty())
    {
        checks.push(check(
            DiagnosticStatus::Warning,
            t("Recent Diagnostic"),
            diagnostic,
            t("Address the failed items above, then re-detect"),
        ));
    }

    checks.sort_by(|left, right| {
        left.status
            .rank()
            .cmp(&right.status.rank())
            .then_with(|| left.title.cmp(&right.title))
    });
    DiagnosticReport { checks }
}

fn check(
    status: DiagnosticStatus,
    title: impl Into<String>,
    detail: impl Into<String>,
    suggestion: impl Into<String>,
) -> DiagnosticCheck {
    DiagnosticCheck {
        status,
        title: title.into(),
        detail: detail.into(),
        suggestion: suggestion.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_report_omits_details_that_may_contain_local_state() {
        let report = DiagnosticReport {
            checks: vec![DiagnosticCheck {
                status: DiagnosticStatus::Failed,
                title: t("Game Connection"),
                detail: r#"IP 192.168.1.2 GUID \Device\NPF_{abc} path C:\Users\me"#.to_owned(),
                suggestion: t("Address the failed items above, then re-detect"),
            }],
        };

        let text = report.redacted_text();

        assert!(text.contains(&t("Address the failed items above, then re-detect")));
        assert!(!text.contains("192.168.1.2"));
        assert!(!text.contains("NPF_"));
        assert!(!text.contains("C:\\Users"));
    }
}
