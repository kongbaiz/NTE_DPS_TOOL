use std::collections::HashSet;

use crate::net_identity::{NetIdentityCandidate, NetIdentityCandidateKind};
use crate::object_state::{is_ignored_non_target_path, is_targetish_path};
use crate::parser::{ParsedSdkTargetHpKind, parse_sdk_target_hp_updates};
use crate::protocol::{SingleBunch, TransportPacket};
use crate::target_instance::{TargetAlias, TargetAliasKind};
use crate::ue_bitstream::{PathCandidate, decode_shifted_bytes};

const TEXT_SCAN_MIN_LEN: usize = 4;
const TEXT_PATH_ASSOCIATION_WINDOW_BYTES: usize = 512;
const MAX_TEXT_EVENTS: usize = 24;
const MAX_IDENTITY_EVENTS: usize = 24;
const MAX_SDK_TARGET_EVENTS: usize = 12;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NetRuntimeEventKind {
    ActorChannel,
    TransportBunch,
    PackageMapExport,
    IrisReference,
    ObjectLifecycle,
    SdkTargetData,
    AbilityLifecycle,
    GameplayEffectLifecycle,
}

impl NetRuntimeEventKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ActorChannel => "actor_channel",
            Self::TransportBunch => "transport_bunch",
            Self::PackageMapExport => "package_map_export",
            Self::IrisReference => "iris_reference",
            Self::ObjectLifecycle => "object_lifecycle",
            Self::SdkTargetData => "sdk_target_data",
            Self::AbilityLifecycle => "ability_lifecycle",
            Self::GameplayEffectLifecycle => "gameplay_effect_lifecycle",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NetRuntimeAction {
    Map,
    Open,
    Close,
    Spawn,
    Destroy,
    UpdateHp,
    Observe,
}

impl NetRuntimeAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Map => "map",
            Self::Open => "open",
            Self::Close => "close",
            Self::Spawn => "spawn",
            Self::Destroy => "destroy",
            Self::UpdateHp => "update_hp",
            Self::Observe => "observe",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetRuntimeTextMarker {
    pub kind: NetRuntimeEventKind,
    pub action: NetRuntimeAction,
    pub text: String,
    pub byte_offset: usize,
    pub bit_shift: u8,
    pub score: u16,
}

#[derive(Clone, Debug)]
pub struct NetRuntimeEvent {
    pub kind: NetRuntimeEventKind,
    pub action: NetRuntimeAction,
    pub aliases: Vec<TargetAlias>,
    pub path: Option<String>,
    pub current_hp: Option<f32>,
    pub dead_state: Option<i32>,
    pub target_token: Option<Vec<u8>>,
    pub byte_offset: usize,
    pub bit_shift: u8,
    pub score: u16,
    pub evidence: String,
}

impl NetRuntimeEvent {
    pub fn summary(&self) -> String {
        let mut parts = vec![format!(
            "{}:{}@{}:{}",
            self.kind.label(),
            self.action.label(),
            self.byte_offset,
            self.bit_shift
        )];
        if let Some(path) = &self.path {
            parts.push(format!("path={path}"));
        }
        if !self.aliases.is_empty() {
            parts.push(format!(
                "aliases={}",
                self.aliases
                    .iter()
                    .map(|alias| format!("{}={}", alias.kind.label(), alias.value))
                    .collect::<Vec<_>>()
                    .join("|")
            ));
        }
        if let Some(current_hp) = self.current_hp {
            parts.push(format!("hp={current_hp:.0}"));
        }
        if let Some(dead_state) = self.dead_state {
            parts.push(format!("dead={dead_state}"));
        }
        parts.push(format!("score={}", self.score));
        parts.join(" ")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NetRuntimeScanOptions {
    pub include_text_markers: bool,
    pub include_sdk_target_data: bool,
}

pub fn extract_net_runtime_events(
    data: &[u8],
    paths: &[PathCandidate],
    identities: &[NetIdentityCandidate],
    transport: Option<&TransportPacket>,
    single_bunch: Option<&SingleBunch>,
    options: NetRuntimeScanOptions,
) -> Vec<NetRuntimeEvent> {
    let mut events = Vec::new();
    events.extend(events_from_identities(identities));
    if let Some(transport) = transport {
        events.extend(events_from_transport(transport, single_bunch));
    }
    if options.include_text_markers {
        events.extend(events_from_text_markers(data, paths));
    }
    if options.include_sdk_target_data {
        events.extend(events_from_sdk_target_data(data, paths));
    }
    dedupe_events(events)
}

pub fn extract_net_runtime_text_markers(data: &[u8]) -> Vec<NetRuntimeTextMarker> {
    let mut markers = Vec::new();
    let mut seen = HashSet::new();
    for bit_shift in 0..8_u8 {
        let Some(shifted) = decode_shifted_bytes(
            data,
            0,
            bit_shift,
            0,
            data.len().saturating_sub(usize::from(bit_shift != 0)),
        ) else {
            continue;
        };
        for (offset, text) in ascii_runs(&shifted) {
            for marker in markers_for_text(text, offset, bit_shift) {
                let key = (
                    marker.kind,
                    marker.action,
                    marker.text.to_ascii_lowercase(),
                    marker.byte_offset,
                    marker.bit_shift,
                );
                if seen.insert(key) {
                    markers.push(marker);
                }
            }
        }
    }
    markers.sort_by_key(|marker| {
        (
            std::cmp::Reverse(marker.score),
            marker.bit_shift,
            marker.byte_offset,
        )
    });
    markers.truncate(MAX_TEXT_EVENTS);
    markers
}

fn events_from_identities(identities: &[NetIdentityCandidate]) -> Vec<NetRuntimeEvent> {
    identities
        .iter()
        .take(MAX_IDENTITY_EVENTS)
        .map(|identity| {
            let (kind, alias_kind) = match identity.kind {
                NetIdentityCandidateKind::NetGuidPacked => (
                    NetRuntimeEventKind::PackageMapExport,
                    TargetAliasKind::NetGuidPacked,
                ),
                NetIdentityCandidateKind::NetGuid32 => (
                    NetRuntimeEventKind::PackageMapExport,
                    TargetAliasKind::NetGuid32,
                ),
                NetIdentityCandidateKind::IrisNetRefHandle32 => (
                    NetRuntimeEventKind::IrisReference,
                    TargetAliasKind::IrisRef32,
                ),
            };
            NetRuntimeEvent {
                kind,
                action: NetRuntimeAction::Map,
                aliases: vec![TargetAlias::new(alias_kind, identity.handle.clone())],
                path: Some(identity.path.clone()),
                current_hp: None,
                dead_state: None,
                target_token: None,
                byte_offset: identity.byte_offset,
                bit_shift: identity.bit_shift,
                score: identity.score,
                evidence: format!(
                    "{}:{} raw={} rel={}",
                    identity.kind.label(),
                    identity.handle,
                    identity.raw_hex,
                    identity.relative_offset
                ),
            }
        })
        .collect()
}

fn events_from_transport(
    transport: &TransportPacket,
    single_bunch: Option<&SingleBunch>,
) -> Vec<NetRuntimeEvent> {
    match transport {
        TransportPacket::StatelessHandshake {
            handler_prefix,
            payload_bit_len,
        } => vec![NetRuntimeEvent {
            kind: NetRuntimeEventKind::TransportBunch,
            action: NetRuntimeAction::Observe,
            aliases: Vec::new(),
            path: None,
            current_hp: None,
            dead_state: None,
            target_token: None,
            byte_offset: 0,
            bit_shift: 0,
            score: 35,
            evidence: format!(
                "stateless_handshake handler_prefix={handler_prefix} payload_bits={payload_bit_len}"
            ),
        }],
        TransportPacket::Sequenced(packet) => {
            let mut events = vec![NetRuntimeEvent {
                kind: NetRuntimeEventKind::TransportBunch,
                action: NetRuntimeAction::Observe,
                aliases: Vec::new(),
                path: None,
                current_hp: None,
                dead_state: None,
                target_token: None,
                byte_offset: 0,
                bit_shift: 0,
                score: 40,
                evidence: format!(
                    "sequenced mode={} packet_id={} ack={} flags={} payload_bits={}",
                    packet.mode,
                    packet.packet_id,
                    packet.acknowledged_packet_id,
                    packet.packet_flags,
                    packet.payload_bit_len
                ),
            }];
            if let Some(bunch) = single_bunch {
                events.push(NetRuntimeEvent {
                    kind: NetRuntimeEventKind::ActorChannel,
                    action: NetRuntimeAction::Observe,
                    aliases: Vec::new(),
                    path: None,
                    current_hp: None,
                    dead_state: None,
                    target_token: None,
                    byte_offset: 0,
                    bit_shift: 0,
                    score: 55,
                    evidence: format!(
                        "single_bunch prefix=0x{:x} sequence={} descriptor=0x{:02x} data_bits={}",
                        bunch.prefix, bunch.sequence, bunch.descriptor, bunch.data_bit_len
                    ),
                });
            }
            events
        }
    }
}

fn events_from_text_markers(data: &[u8], paths: &[PathCandidate]) -> Vec<NetRuntimeEvent> {
    extract_net_runtime_text_markers(data)
        .into_iter()
        .map(|marker| {
            let path = nearest_target_path(paths, marker.byte_offset, marker.bit_shift)
                .map(|candidate| candidate.value.clone());
            NetRuntimeEvent {
                kind: marker.kind,
                action: marker.action,
                aliases: Vec::new(),
                path,
                current_hp: None,
                dead_state: None,
                target_token: None,
                byte_offset: marker.byte_offset,
                bit_shift: marker.bit_shift,
                score: marker.score,
                evidence: marker.text,
            }
        })
        .collect()
}

fn events_from_sdk_target_data(data: &[u8], paths: &[PathCandidate]) -> Vec<NetRuntimeEvent> {
    let mut events = parse_sdk_target_hp_updates(data)
        .into_iter()
        .take(MAX_SDK_TARGET_EVENTS)
        .map(|update| {
            let token = update.target_token.to_vec();
            let token_hex = hex::encode(&token);
            let path = nearest_target_path(paths, update.byte_offset, update.bit_shift)
                .map(|candidate| candidate.value.clone());
            let score = if path.is_some() { 78 } else { 52 };
            NetRuntimeEvent {
                kind: NetRuntimeEventKind::SdkTargetData,
                action: NetRuntimeAction::UpdateHp,
                aliases: vec![TargetAlias::new(TargetAliasKind::SdkNetTarget, &token_hex)],
                path,
                current_hp: Some(update.current_hp),
                dead_state: Some(update.dead_state),
                target_token: Some(token),
                byte_offset: update.byte_offset,
                bit_shift: update.bit_shift,
                score,
                evidence: format!(
                    "{} current_hp={:.0} dead_state={}",
                    match update.kind {
                        ParsedSdkTargetHpKind::ClientRepExtraDamageInfo =>
                            "FClientRepExtraDamageInfo",
                        ParsedSdkTargetHpKind::ClientRepFightData => "FClientRepFightData",
                    },
                    update.current_hp,
                    update.dead_state
                ),
            }
        })
        .collect::<Vec<_>>();
    events.sort_by_key(|event| {
        (
            std::cmp::Reverse(event.score),
            event.bit_shift,
            event.byte_offset,
        )
    });
    events
}

fn markers_for_text(text: &str, byte_offset: usize, bit_shift: u8) -> Vec<NetRuntimeTextMarker> {
    let lower = text.to_ascii_lowercase();
    let mut markers = Vec::new();
    let mut push =
        |needle: &str, kind: NetRuntimeEventKind, action: NetRuntimeAction, score: u16| {
            if lower.contains(&needle.to_ascii_lowercase()) {
                markers.push(NetRuntimeTextMarker {
                    kind,
                    action,
                    text: needle.to_owned(),
                    byte_offset,
                    bit_shift,
                    score,
                });
            }
        };

    push(
        "ClientSetReplicatedTargetData",
        NetRuntimeEventKind::SdkTargetData,
        NetRuntimeAction::Map,
        92,
    );
    push(
        "ClientUpdateTargetExtraDamageInfos",
        NetRuntimeEventKind::SdkTargetData,
        NetRuntimeAction::UpdateHp,
        92,
    );
    push(
        "ClientServerAbilityActorSetCurrentHP",
        NetRuntimeEventKind::SdkTargetData,
        NetRuntimeAction::UpdateHp,
        88,
    );
    push(
        "FClientReplicatedTargetDataContainer",
        NetRuntimeEventKind::SdkTargetData,
        NetRuntimeAction::Map,
        84,
    );
    push(
        "FClientRepExtraDamageInfo",
        NetRuntimeEventKind::SdkTargetData,
        NetRuntimeAction::UpdateHp,
        84,
    );
    push(
        "FClientRepFightData",
        NetRuntimeEventKind::SdkTargetData,
        NetRuntimeAction::UpdateHp,
        84,
    );
    push(
        "NetMulticast_OnSendHandleDamageInfos",
        NetRuntimeEventKind::AbilityLifecycle,
        NetRuntimeAction::Observe,
        88,
    );
    push(
        "ServerReceiveGameplayEventToActor",
        NetRuntimeEventKind::AbilityLifecycle,
        NetRuntimeAction::Observe,
        82,
    );
    push(
        "ServerTriggerAbilityNextSection",
        NetRuntimeEventKind::AbilityLifecycle,
        NetRuntimeAction::Observe,
        82,
    );
    push(
        "ServerSpawnProjectile",
        NetRuntimeEventKind::AbilityLifecycle,
        NetRuntimeAction::Spawn,
        82,
    );
    push(
        "NetMulticast_OnSendPlayGamePlayEffect",
        NetRuntimeEventKind::GameplayEffectLifecycle,
        NetRuntimeAction::Observe,
        88,
    );
    push(
        "GameplayCue",
        NetRuntimeEventKind::GameplayEffectLifecycle,
        NetRuntimeAction::Observe,
        70,
    );
    push(
        "GameplayEffect",
        NetRuntimeEventKind::GameplayEffectLifecycle,
        NetRuntimeAction::Observe,
        64,
    );
    push(
        "ActorChannel",
        NetRuntimeEventKind::ActorChannel,
        NetRuntimeAction::Observe,
        78,
    );
    push(
        "OpenChannel",
        NetRuntimeEventKind::ActorChannel,
        NetRuntimeAction::Open,
        78,
    );
    push(
        "OpenedChannel",
        NetRuntimeEventKind::ActorChannel,
        NetRuntimeAction::Open,
        78,
    );
    push(
        "PackageMap",
        NetRuntimeEventKind::PackageMapExport,
        NetRuntimeAction::Map,
        82,
    );
    push(
        "NetGUID",
        NetRuntimeEventKind::PackageMapExport,
        NetRuntimeAction::Map,
        76,
    );
    push(
        "NetGuid",
        NetRuntimeEventKind::PackageMapExport,
        NetRuntimeAction::Map,
        76,
    );
    push(
        "NetRefHandle",
        NetRuntimeEventKind::IrisReference,
        NetRuntimeAction::Map,
        82,
    );
    push(
        "Iris",
        NetRuntimeEventKind::IrisReference,
        NetRuntimeAction::Map,
        64,
    );
    push(
        "SpawnActor",
        NetRuntimeEventKind::ObjectLifecycle,
        NetRuntimeAction::Spawn,
        82,
    );
    push(
        "BeginPlay",
        NetRuntimeEventKind::ObjectLifecycle,
        NetRuntimeAction::Spawn,
        64,
    );
    push(
        "Destroyed",
        NetRuntimeEventKind::ObjectLifecycle,
        NetRuntimeAction::Destroy,
        82,
    );
    push(
        "Destroy",
        NetRuntimeEventKind::ObjectLifecycle,
        NetRuntimeAction::Destroy,
        74,
    );
    push(
        "TearOff",
        NetRuntimeEventKind::ObjectLifecycle,
        NetRuntimeAction::Destroy,
        82,
    );
    push(
        "Dormancy",
        NetRuntimeEventKind::ObjectLifecycle,
        NetRuntimeAction::Close,
        70,
    );

    markers
}

fn nearest_target_path(
    paths: &[PathCandidate],
    byte_offset: usize,
    bit_shift: u8,
) -> Option<&PathCandidate> {
    paths
        .iter()
        .filter(|path| path.bit_shift == bit_shift)
        .filter(|path| !is_ignored_non_target_path(&path.value) && is_targetish_path(&path.value))
        .filter(|path| path.byte_offset.abs_diff(byte_offset) <= TEXT_PATH_ASSOCIATION_WINDOW_BYTES)
        .max_by_key(|path| {
            (
                path.score,
                std::cmp::Reverse(path.byte_offset.abs_diff(byte_offset)),
            )
        })
}

fn ascii_runs(data: &[u8]) -> Vec<(usize, &str)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (index, byte) in data.iter().enumerate() {
        if (0x20..=0x7e).contains(byte) {
            start.get_or_insert(index);
            continue;
        }
        if let Some(run_start) = start.take()
            && index - run_start >= TEXT_SCAN_MIN_LEN
            && let Ok(value) = std::str::from_utf8(&data[run_start..index])
        {
            runs.push((run_start, value.trim()));
        }
    }
    if let Some(run_start) = start
        && data.len() - run_start >= TEXT_SCAN_MIN_LEN
        && let Ok(value) = std::str::from_utf8(&data[run_start..])
    {
        runs.push((run_start, value.trim()));
    }
    runs
}

fn dedupe_events(events: Vec<NetRuntimeEvent>) -> Vec<NetRuntimeEvent> {
    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for event in events {
        let key = (
            event.kind,
            event.action,
            event.path.clone(),
            event
                .aliases
                .iter()
                .map(TargetAlias::key)
                .collect::<Vec<_>>()
                .join("|"),
            event.current_hp.map(f32::to_bits),
            event.dead_state,
            event.byte_offset,
            event.bit_shift,
        );
        if seen.insert(key) {
            deduped.push(event);
        }
    }
    deduped.sort_by_key(|event| {
        (
            std::cmp::Reverse(event.score),
            event.kind.label(),
            event.bit_shift,
            event.byte_offset,
        )
    });
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str, byte_offset: usize, bit_shift: u8) -> PathCandidate {
        PathCandidate {
            value: value.to_owned(),
            byte_offset,
            bit_shift,
            score: 240,
        }
    }

    fn identity(kind: NetIdentityCandidateKind, path: &str, handle: &str) -> NetIdentityCandidate {
        NetIdentityCandidate {
            kind,
            handle: handle.to_owned(),
            path: path.to_owned(),
            byte_offset: 12,
            bit_shift: 0,
            relative_offset: -8,
            raw_hex: "78563412".to_owned(),
            score: 90,
        }
    }

    #[test]
    fn builds_package_map_events_from_net_identities() {
        let events = extract_net_runtime_events(
            &[],
            &[],
            &[identity(
                NetIdentityCandidateKind::NetGuid32,
                "WorldBoss_Boss33",
                "0x12345678",
            )],
            None,
            None,
            NetRuntimeScanOptions::default(),
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, NetRuntimeEventKind::PackageMapExport);
        assert_eq!(events[0].aliases[0].kind, TargetAliasKind::NetGuid32);
    }

    #[test]
    fn detects_lifecycle_text_near_target_path() {
        let payload = b"SpawnActor WorldBoss_Boss33";
        let events = extract_net_runtime_events(
            payload,
            &[path("WorldBoss_Boss33", 11, 0)],
            &[],
            None,
            None,
            NetRuntimeScanOptions {
                include_text_markers: true,
                include_sdk_target_data: false,
            },
        );

        assert!(events.iter().any(|event| {
            event.kind == NetRuntimeEventKind::ObjectLifecycle
                && event.action == NetRuntimeAction::Spawn
                && event.path.as_deref() == Some("WorldBoss_Boss33")
        }));
    }

    #[test]
    fn detects_sdk_target_hp_data() {
        let mut token = [0_u8; 0x28];
        for (index, byte) in token.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_add(1);
        }
        let mut payload = Vec::new();
        payload.extend(token);
        payload.extend(1234.0_f32.to_le_bytes());
        payload.extend(0_i32.to_le_bytes());

        let events = extract_net_runtime_events(
            &payload,
            &[],
            &[],
            None,
            None,
            NetRuntimeScanOptions {
                include_text_markers: false,
                include_sdk_target_data: true,
            },
        );

        assert!(events.iter().any(|event| {
            event.kind == NetRuntimeEventKind::SdkTargetData
                && event.current_hp == Some(1234.0)
                && event.aliases[0].kind == TargetAliasKind::SdkNetTarget
        }));
    }
}
