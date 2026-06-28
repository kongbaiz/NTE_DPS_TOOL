use crate::capture::list_devices;
use crate::network::detect_game_device;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiagnosticStatus {
    Passed,
    #[default]
    Warning,
    Failed,
}

impl DiagnosticStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "通过",
            Self::Warning => "警告",
            Self::Failed => "失败",
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
        text.push_str("NTE DPS TOOL 自动诊断报告\n");
        text.push_str(&format!(
            "失败 {}，警告 {}\n",
            self.failed_count(),
            self.warning_count()
        ));
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
                "Npcap 设备",
                "Npcap 已加载，但没有返回可用抓包设备",
                "确认 Npcap 安装完整，并尝试以管理员身份运行",
            ));
            checks.push(check(
                DiagnosticStatus::Failed,
                "游戏连接",
                "没有可匹配的抓包设备，无法定位游戏连接",
                "先修复 Npcap 设备枚举，再进入游戏场景后重新诊断",
            ));
        }
        Ok(devices) => {
            checks.push(check(
                DiagnosticStatus::Passed,
                "Npcap 设备",
                format!("检测到 {} 个可用设备", devices.len()),
                "设备枚举正常",
            ));
            match detect_game_device(&devices) {
                Ok((_, network)) => checks.push(check(
                    DiagnosticStatus::Passed,
                    "游戏连接",
                    format!("已定位 HTGame.exe PID {}", network.pid),
                    "已检测到 HTGame.exe 活动连接和匹配网卡",
                )),
                Err(error) => checks.push(check(
                    DiagnosticStatus::Failed,
                    "游戏连接",
                    error,
                    "未检测到 HTGame.exe 活动连接，请先进入游戏场景后再开始抓包",
                )),
            }
        }
        Err(error) => {
            checks.push(check(
                DiagnosticStatus::Failed,
                "Npcap 设备",
                error,
                "请安装 Npcap，并确认 WinPcap API-compatible Mode 可用",
            ));
            checks.push(check(
                DiagnosticStatus::Failed,
                "游戏连接",
                "Npcap 不可用，跳过游戏连接定位",
                "先修复 Npcap 加载问题，再重新运行诊断",
            ));
        }
    }

    if snapshot.capture_running {
        checks.push(check(
            DiagnosticStatus::Passed,
            "抓包状态",
            snapshot.active_capture_filter.as_deref().map_or_else(
                || "实时抓包已启动，BPF 正在确定".to_owned(),
                |filter| format!("实时抓包已启动，BPF={filter}"),
            ),
            "实时抓包任务正在运行",
        ));
    } else if snapshot.replay_running {
        checks.push(check(
            DiagnosticStatus::Passed,
            "抓包状态",
            "正在导入回放",
            "回放导入中，实时抓包检查不适用",
        ));
    } else {
        checks.push(check(
            DiagnosticStatus::Warning,
            "抓包状态",
            "当前没有实时抓包任务",
            "点击开始后再运行诊断，可以看到 BPF 和原始抓包写入状态",
        ));
    }

    if snapshot.raw_packet_count > 0 {
        checks.push(check(
            DiagnosticStatus::Passed,
            "原始抓包",
            format!("已写入 {} 个原始包", snapshot.raw_packet_count),
            "原始 PCAPNG 写入正常",
        ));
    } else if snapshot.capture_running {
        checks.push(check(
            DiagnosticStatus::Warning,
            "原始抓包",
            "抓包运行中，但尚未写入原始包",
            "确认游戏处于联网场景，必要时收窄或恢复默认 BPF",
        ));
    } else {
        checks.push(check(
            DiagnosticStatus::Warning,
            "原始抓包",
            "当前没有可用原始包",
            "开始抓包并等待游戏产生网络流量后再检查",
        ));
    }

    if snapshot.hit_count > 0 {
        checks.push(check(
            DiagnosticStatus::Passed,
            "伤害解析",
            format!("已解析 {} 条伤害", snapshot.hit_count),
            "伤害解析已有结果",
        ));
    } else if snapshot.parsed_packet_count > 0 || snapshot.raw_packet_count > 0 {
        checks.push(check(
            DiagnosticStatus::Warning,
            "伤害解析",
            format!(
                "已有 {} 个解析封包，但暂无伤害",
                snapshot.parsed_packet_count
            ),
            "进入战斗并造成伤害；若仍为 0，请导入 PCAPNG 到诊断页复核",
        ));
    } else {
        checks.push(check(
            DiagnosticStatus::Warning,
            "伤害解析",
            "尚无封包和伤害数据",
            "先进入游戏场景并开始抓包，确认状态栏不再提示采集错误",
        ));
    }

    checks.push(check(
        if snapshot.include_incoming {
            DiagnosticStatus::Passed
        } else {
            DiagnosticStatus::Warning
        },
        "受击记录",
        if snapshot.include_incoming {
            "受击解析已启用"
        } else {
            "受击解析未启用"
        },
        if snapshot.include_incoming {
            "受击统计会进入解析质量报告"
        } else {
            "如需排查方向判定，请启用受击记录后重新抓包"
        },
    ));

    checks.push(check(
        DiagnosticStatus::Passed,
        "服务端校准",
        if snapshot.server_damage_calibration {
            "服务端 HP 差值校准已启用"
        } else {
            "服务端 HP 差值校准未启用"
        },
        if snapshot.server_damage_calibration {
            "伤害值会在能明确配对时使用服务端 HP 差值"
        } else {
            "如需排查伤害数值偏差，可启用校准后重新抓包或导入"
        },
    ));

    if let Some(diagnostic) = snapshot
        .last_diagnostic
        .filter(|diagnostic| !diagnostic.trim().is_empty())
    {
        checks.push(check(
            DiagnosticStatus::Warning,
            "最近诊断",
            diagnostic,
            "按上方失败项处理后重新检测",
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
                title: "游戏连接".to_owned(),
                detail: r#"IP 192.168.1.2 GUID \Device\NPF_{abc} path C:\Users\me"#.to_owned(),
                suggestion: "进入游戏场景后重新诊断".to_owned(),
            }],
        };

        let text = report.redacted_text();

        assert!(text.contains("进入游戏场景后重新诊断"));
        assert!(!text.contains("192.168.1.2"));
        assert!(!text.contains("NPF_"));
        assert!(!text.contains("C:\\Users"));
    }
}
