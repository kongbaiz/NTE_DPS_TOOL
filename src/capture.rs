use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_uint};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use chrono::Local;
use crossbeam_channel::Sender;
use libloading::Library;
use pcap_file::DataLink;
use pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock;
use pcap_file::pcapng::blocks::interface_description::{
    InterfaceDescriptionBlock, InterfaceDescriptionOption,
};
use pcap_file::pcapng::{Block, PcapNgReader, PcapNgWriter};
use serde::Deserialize;

use crate::model::{
    AbyssEvent, AbyssHalf, CharacterInfo, EngineEvent, Hit, HitTargetUpdate, PacketDebug,
    PacketDebugMode, stable_hit_uid,
};
use crate::net_event::{
    NetRuntimeAction, NetRuntimeEvent, NetRuntimeEventKind, NetRuntimeScanOptions,
    extract_net_runtime_events,
};
use crate::net_identity::{NetIdentityCandidateKind, extract_net_identity_candidates};
use crate::object_state::{
    ObjectHandleKind, ObjectStateStore, is_ignored_non_target_path, is_targetish_path,
};
use crate::parser::{
    GAMEPLAY_EFFECT_MAPPING_PATH, GameplayEffectSkill, ParsedBossHpUpdate, ParsedCurrentHpUpdate,
    ParsedGameplayEffect, SKILL_DAMAGE_DATA_PATH, WOODEN_DAMAGE_DESCRIPTIONS_PATH,
    classify_attack_type, classify_attack_type_from_description,
    declared_character_ids_from_evidence, find_data_file, find_declared_character_evidence,
    load_gameplay_effect_mapping, load_gameplay_effect_skills, load_wooden_damage_names,
    normalize_damage_name, parse_boss_hp_updates, parse_current_hp_updates, parse_damage_payload,
    parse_gameplay_effects, qte_reaction_type,
};

use crate::protocol::{TransportPacket, parse_single_bunch, parse_transport_packet};
use crate::resource_index::ResourceIndex;
use crate::runtime_mapping::{
    RuntimeMappingAction, RuntimeMappingEvent, RuntimeMappingTimeline,
    find_companion_runtime_mapping_sidecar, load_runtime_mapping_sidecar,
};
use crate::target_alias::{
    equivalent_target_alias_keys, is_hp_alias_key, is_legacy_handle_candidate_id,
    target_alias_lookup_keys, target_context_value, target_context_values,
};
use crate::target_instance::{TargetAlias, TargetAliasKind, TargetInstanceStore};
use crate::target_resolver::{TargetConfidence, TargetResolutionSummary, TargetResolver};
use crate::target_track::{TargetTrackStore, TrackPacketContext};
use crate::ue_bitstream::{PathCandidate, extract_path_candidates};

const PCAP_ERRBUF_SIZE: usize = 256;
const MIN_READABLE_TEXT_LEN: usize = 4;
const MAX_IGNORABLE_BINARY_PACKET_LEN: usize = 96;
const MIN_POSSIBLE_DAMAGE_PACKET_LEN: usize = 88;
const DAMAGE_RECORD_MIN_LEN: usize = 94;
const DAMAGE_FIELD_F32_PREFIX: [u8; 5] = [12, 4, 0, 0, 0];
const DAMAGE_FIELD_F64_PREFIX: [u8; 5] = [13, 8, 0, 0, 0];
const FAST_SKIP_TEXT_HINTS: &[&[u8]] = &[
    b"/Game/",
    b"mon_",
    b"_BP",
    b"Boss",
    b"Gameplay",
    b"Ability",
    b"FHTClient",
    b"ClientRep",
    b"CurrentHP",
    b"Damage",
    b"ActorChannel",
    b"PackageMap",
    b"NetGUID",
    b"Iris",
];
const UNREADABLE_PROTOCOL_TEXT: &str = "未解析到可读协议文本";
const CAPTURE_SNAPLEN: u32 = 65_535;
const RAW_CAPTURE_FLUSH_INTERVAL: u64 = 256;
const DEAD_TARGET_HANDLE_REUSE_WINDOW_SECONDS: f64 = 8.0;

#[repr(C)]
struct PcapIf {
    next: *mut PcapIf,
    name: *mut c_char,
    description: *mut c_char,
    addresses: *mut PcapAddr,
    flags: c_uint,
}

#[repr(C)]
struct PcapAddr {
    next: *mut PcapAddr,
    addr: *mut SockAddr,
    netmask: *mut SockAddr,
    broadaddr: *mut SockAddr,
    dstaddr: *mut SockAddr,
}

#[repr(C)]
struct SockAddr {
    family: u16,
    data: [u8; 14],
}

#[repr(C)]
struct TimeVal {
    tv_sec: i32,
    tv_usec: i32,
}

#[repr(C)]
struct PcapPkthdr {
    ts: TimeVal,
    caplen: c_uint,
    len: c_uint,
}

#[repr(C)]
struct BpfProgram {
    bf_len: c_uint,
    bf_insns: *mut std::ffi::c_void,
}

type PcapT = std::ffi::c_void;
type FindAllDevs = unsafe extern "C" fn(*mut *mut PcapIf, *mut c_char) -> c_int;
type FreeAllDevs = unsafe extern "C" fn(*mut PcapIf);
type OpenLive = unsafe extern "C" fn(*const c_char, c_int, c_int, c_int, *mut c_char) -> *mut PcapT;
type NextEx =
    unsafe extern "C" fn(*mut PcapT, *mut *const PcapPkthdr, *mut *const c_uchar) -> c_int;
type Close = unsafe extern "C" fn(*mut PcapT);
type Compile =
    unsafe extern "C" fn(*mut PcapT, *mut BpfProgram, *const c_char, c_int, c_uint) -> c_int;
type SetFilter = unsafe extern "C" fn(*mut PcapT, *mut BpfProgram) -> c_int;
type FreeCode = unsafe extern "C" fn(*mut BpfProgram);
type GetErr = unsafe extern "C" fn(*mut PcapT) -> *const c_char;

struct PcapHandle {
    raw: *mut PcapT,
    close: Close,
}

impl PcapHandle {
    fn new(raw: *mut PcapT, close: Close) -> Self {
        Self { raw, close }
    }

    fn as_ptr(&self) -> *mut PcapT {
        self.raw
    }
}

impl Drop for PcapHandle {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                (self.close)(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

struct BpfProgramGuard {
    program: BpfProgram,
    free_code: FreeCode,
    active: bool,
}

impl BpfProgramGuard {
    fn new(free_code: FreeCode) -> Self {
        Self {
            program: BpfProgram {
                bf_len: 0,
                bf_insns: ptr::null_mut(),
            },
            free_code,
            active: true,
        }
    }

    fn as_mut(&mut self) -> &mut BpfProgram {
        &mut self.program
    }

    fn release(&mut self) {
        if self.active {
            unsafe {
                (self.free_code)(&mut self.program);
            }
            self.active = false;
        }
    }
}

impl Drop for BpfProgramGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone, Debug)]
pub struct CaptureDevice {
    pub name: String,
    pub description: String,
    pub ipv4: Vec<Ipv4Addr>,
}

pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    raw_capture: Option<RawCaptureBuffer>,
}

impl CaptureHandle {
    pub fn raw_capture(&self) -> Option<RawCaptureBuffer> {
        self.raw_capture.clone()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone)]
pub struct RawCaptureBuffer {
    inner: Arc<Mutex<RawCaptureData>>,
}

struct RawCaptureData {
    path: Option<PathBuf>,
    writer: Option<RawCaptureWriter>,
    packet_count: u64,
    captured_bytes: u64,
    write_error: Option<String>,
}

impl RawCaptureBuffer {
    fn disabled() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RawCaptureData {
                path: None,
                writer: None,
                packet_count: 0,
                captured_bytes: 0,
                write_error: None,
            })),
        }
    }

    fn new(device: CaptureDevice) -> Self {
        let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f");
        let path = PathBuf::from(format!("logs/nte_raw_{timestamp}.pcapng"));
        let writer = RawCaptureWriter::create(&path, &device);
        let (writer, write_error) = match writer {
            Ok(writer) => (Some(writer), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            inner: Arc::new(Mutex::new(RawCaptureData {
                path: Some(path),
                writer,
                packet_count: 0,
                captured_bytes: 0,
                write_error,
            })),
        }
    }

    fn push(&self, timestamp: Duration, original_len: u32, packet: &[u8]) {
        if let Ok(mut capture) = self.inner.lock() {
            let result = capture
                .writer
                .as_mut()
                .map(|writer| writer.write_packet(timestamp, original_len, packet));
            match result {
                Some(Ok(())) => {
                    capture.packet_count += 1;
                    capture.captured_bytes += packet.len() as u64;
                }
                Some(Err(error)) => {
                    capture.write_error = Some(error);
                    capture.writer = None;
                }
                None => {}
            }
        }
    }

    pub fn packet_count(&self) -> usize {
        self.inner
            .lock()
            .map_or(0, |capture| capture.packet_count as usize)
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .ok()
            .filter(|capture| capture.write_error.is_none())
            .and_then(|capture| capture.path.clone())
    }

    fn finish(&self) {
        let Ok(mut capture) = self.inner.lock() else {
            return;
        };
        if let Some(writer) = capture.writer.take()
            && let Err(error) = writer.finish()
        {
            capture.write_error = Some(error);
        }
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(u64, u64), String> {
        let capture = self
            .inner
            .lock()
            .map_err(|_| "raw capture lock poisoned".to_owned())?;
        if capture.writer.is_some() {
            return Err("raw capture is still being written; stop capture first".to_owned());
        }
        if let Some(error) = &capture.write_error {
            return Err(format!("raw capture write failed: {error}"));
        }
        let Some(source_path) = capture.path.as_ref() else {
            return Err("raw capture was not enabled for this session".to_owned());
        };
        if path != source_path {
            std::fs::copy(source_path, path).map_err(|error| {
                format!(
                    "failed to copy raw capture {} to {}: {error}",
                    source_path.display(),
                    path.display()
                )
            })?;
        }
        Ok((capture.packet_count, capture.captured_bytes))
    }
}

struct RawCaptureWriter {
    writer: PcapNgWriter<BufWriter<File>>,
    packet_count: u64,
    captured_bytes: u64,
}

impl RawCaptureWriter {
    fn create(path: &std::path::Path, device: &CaptureDevice) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create raw capture directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let file = File::create(path).map_err(|error| {
            format!(
                "failed to create raw capture file {}: {error}",
                path.display()
            )
        })?;
        let mut writer =
            PcapNgWriter::new(BufWriter::new(file)).map_err(|error| error.to_string())?;
        let mut interface = InterfaceDescriptionBlock::new(DataLink::ETHERNET, CAPTURE_SNAPLEN);
        interface
            .options
            .push(InterfaceDescriptionOption::IfName(Cow::Owned(
                device.name.clone(),
            )));
        if !device.description.is_empty() {
            interface
                .options
                .push(InterfaceDescriptionOption::IfDescription(Cow::Owned(
                    device.description.clone(),
                )));
        }
        // EnhancedPacketBlock stores Duration as nanoseconds.
        interface
            .options
            .push(InterfaceDescriptionOption::IfTsResol(9));
        writer
            .write_pcapng_block(interface)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            writer,
            packet_count: 0,
            captured_bytes: 0,
        })
    }

    fn write_packet(
        &mut self,
        timestamp: Duration,
        original_len: u32,
        packet: &[u8],
    ) -> Result<(), String> {
        self.writer
            .write_pcapng_block(EnhancedPacketBlock {
                interface_id: 0,
                timestamp,
                original_len,
                data: Cow::Borrowed(packet),
                options: Vec::new(),
            })
            .map_err(|error| error.to_string())?;
        self.packet_count += 1;
        self.captured_bytes += packet.len() as u64;
        if self.packet_count & (RAW_CAPTURE_FLUSH_INTERVAL - 1) == 0 {
            self.writer
                .get_mut()
                .flush()
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(u64, u64), String> {
        self.writer
            .get_mut()
            .flush()
            .map_err(|error| error.to_string())?;
        Ok((self.packet_count, self.captured_bytes))
    }
}

fn npcap_library_path() -> PathBuf {
    windows_system_directory().join("Npcap").join("wpcap.dll")
}

fn packet_library_path() -> PathBuf {
    windows_system_directory().join("Npcap").join("Packet.dll")
}

fn windows_system_directory() -> PathBuf {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
}

unsafe fn load_symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, String> {
    // SAFETY: The requested names and signatures match the public libpcap API.
    unsafe {
        library
            .get::<T>(name)
            .map(|symbol| *symbol)
            .map_err(|error| error.to_string())
    }
}

fn c_string(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        // SAFETY: libpcap returns null-terminated strings valid during this call.
        unsafe { CStr::from_ptr(value).to_string_lossy().into_owned() }
    }
}

pub fn list_devices() -> Result<Vec<CaptureDevice>, String> {
    // SAFETY: Loading a known Npcap DLL and calling its documented API.
    unsafe {
        let _packet_library = Library::new(packet_library_path())
            .map_err(|error| format!("无法加载 Npcap Packet.dll: {error}"))?;
        let library = Library::new(npcap_library_path())
            .map_err(|error| format!("无法加载 Npcap，请先安装 Npcap: {error}"))?;
        let find_all_devs: FindAllDevs = load_symbol(&library, b"pcap_findalldevs\0")?;
        let free_all_devs: FreeAllDevs = load_symbol(&library, b"pcap_freealldevs\0")?;
        let mut devices_ptr = ptr::null_mut();
        let mut error_buffer = [0_i8; PCAP_ERRBUF_SIZE];
        if find_all_devs(&mut devices_ptr, error_buffer.as_mut_ptr()) != 0 {
            return Err(c_string(error_buffer.as_ptr()));
        }
        let mut result = Vec::new();
        let mut current = devices_ptr;
        while !current.is_null() {
            let device = &*current;
            let mut ipv4 = Vec::new();
            let mut address = device.addresses;
            while !address.is_null() {
                let addr = (*address).addr;
                if !addr.is_null() && (*addr).family == 2 {
                    let bytes = &(*addr).data;
                    ipv4.push(Ipv4Addr::new(bytes[2], bytes[3], bytes[4], bytes[5]));
                }
                address = (*address).next;
            }
            result.push(CaptureDevice {
                name: c_string(device.name),
                description: c_string(device.description),
                ipv4,
            });
            current = device.next;
        }
        free_all_devs(devices_ptr);
        Ok(result)
    }
}

fn parse_udp_ipv4(packet: &[u8]) -> Option<(Ipv4Addr, u16, Ipv4Addr, u16, &[u8])> {
    if packet.len() < 14 {
        return None;
    }
    let mut ethernet_offset = 14;
    let mut ether_type = u16::from_be_bytes([packet[12], packet[13]]);
    if ether_type == 0x8100 && packet.len() >= 18 {
        ether_type = u16::from_be_bytes([packet[16], packet[17]]);
        ethernet_offset = 18;
    }
    if ether_type != 0x0800 || packet.len() < ethernet_offset + 20 {
        return None;
    }
    let ip = &packet[ethernet_offset..];
    let ip_header_len = ((ip[0] & 0x0f) as usize) * 4;
    let total_len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    let fragment = u16::from_be_bytes([ip[6], ip[7]]);
    if ip[0] >> 4 != 4
        || ip_header_len < 20
        || total_len < ip_header_len + 8
        || ip.len() < total_len
        || ip[9] != 17
        || fragment & 0x3fff != 0
    {
        return None;
    }
    let ip = &ip[..total_len];
    let source = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let destination = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
    let udp = &ip[ip_header_len..];
    let source_port = u16::from_be_bytes([udp[0], udp[1]]);
    let destination_port = u16::from_be_bytes([udp[2], udp[3]]);
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if udp_len < 8 || udp_len > udp.len() {
        return None;
    }
    Some((
        source,
        source_port,
        destination,
        destination_port,
        &udp[8..udp_len],
    ))
}

fn infer_outgoing(
    src: Ipv4Addr,
    src_port: u16,
    dst: Ipv4Addr,
    local_ip: Option<Ipv4Addr>,
    ids: &[u32],
    client_endpoints: &HashSet<(Ipv4Addr, u16)>,
) -> bool {
    if let Some(local_ip) = local_ip {
        return src == local_ip;
    }
    match (src.is_private(), dst.is_private()) {
        (true, false) => true,
        (false, true) => false,
        _ => ids.len() == 1 || client_endpoints.contains(&(src, src_port)),
    }
}

fn decode_shifted_payload(data: &[u8], bit_shift: u8) -> Vec<u8> {
    if bit_shift == 0 {
        return data.to_vec();
    }
    data.windows(2)
        .map(|pair| (pair[0] >> bit_shift) | (pair[1] << (8 - bit_shift)))
        .collect()
}

struct PayloadScan {
    shifted: [Vec<u8>; 8],
}

impl PayloadScan {
    fn new(payload: &[u8]) -> Self {
        Self {
            shifted: std::array::from_fn(|bit_shift| {
                decode_shifted_payload(payload, bit_shift as u8)
            }),
        }
    }

    fn contains_any_fast_hint(&self) -> bool {
        self.shifted.iter().any(|shifted| {
            FAST_SKIP_TEXT_HINTS.iter().any(|hint| {
                hint.len() <= shifted.len()
                    && shifted
                        .windows(hint.len())
                        .any(|window| window.eq_ignore_ascii_case(hint))
            })
        })
    }

    fn decode_text(&self) -> String {
        let mut found = Vec::<(usize, String)>::new();
        for shifted in &self.shifted {
            for (score, value) in extract_length_prefixed_identifiers(shifted) {
                if !found.iter().any(|(_, item)| item == &value) {
                    found.push((score, value));
                }
            }
            for bytes in shifted.split(|byte| !(0x20..=0x7e).contains(byte)) {
                if bytes.len() < MIN_READABLE_TEXT_LEN {
                    continue;
                }
                let Ok(value) = std::str::from_utf8(bytes) else {
                    continue;
                };
                let value = value.trim();
                let score = protocol_text_score(value);
                if score == 0 || found.iter().any(|(_, item)| item == value) {
                    continue;
                }
                found.push((score, value.to_owned()));
            }
        }
        if found.is_empty() {
            UNREADABLE_PROTOCOL_TEXT.to_owned()
        } else {
            found.sort_by_key(|item| std::cmp::Reverse(item.0));
            found
                .into_iter()
                .map(|(_, value)| value)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    fn may_contain_damage_record(&self) -> bool {
        self.shifted.iter().any(|shifted| {
            if shifted.len() < 32 {
                return false;
            }
            (0..=shifted.len() - 32).any(|offset| {
                shifted[offset..offset + 5] == DAMAGE_FIELD_F32_PREFIX
                    && shifted[offset + 9..offset + 14] == DAMAGE_FIELD_F32_PREFIX
                    && shifted[offset + 18..offset + 23] == DAMAGE_FIELD_F32_PREFIX
                    && shifted[offset + 27..offset + 32] == DAMAGE_FIELD_F64_PREFIX
            })
        })
    }
}

fn protocol_text_score(value: &str) -> usize {
    let length = value.len();
    if length < MIN_READABLE_TEXT_LEN {
        return 0;
    }
    let letters = value.bytes().filter(u8::is_ascii_alphabetic).count();
    let digits = value.bytes().filter(u8::is_ascii_digit).count();
    let spaces = value.bytes().filter(|byte| *byte == b' ').count();
    let punctuation = length.saturating_sub(letters + digits + spaces);
    let protocol_markers = [
        "Abyss",
        "Ability.",
        "AbilitySystem",
        "AppearMelee",
        "BackEvade",
        "Boss",
        "CharacterForNet",
        "ClientRepExtraDamageInfo",
        "ClientRepFightData",
        "ClientServerAbilityActorSetCurrentHP",
        "ClientShowPlayerDamageInfo",
        "ClientShowWoodenInfo",
        "ClientUpdateTargetExtraDamageInfos",
        "CityEvent",
        "CityLive",
        "CoolDown.",
        "CurrentGameplayID",
        "DataLayer",
        "DissolveMontage",
        "DropBox",
        "Event.",
        "FrontEvade",
        "Game/",
        "GameplayCue.",
        "HTClient",
        "HTRoom",
        "Monster",
        "PrivateSpawn",
        "Record",
        "SilentCheckComponent",
        "SkeletalMesh",
        "Stamina",
        "State.",
        "Teleport",
        "UnbalCurrent",
        "WorldBoss",
        "FirstHalf",
        "SecondHalf",
        "Phase",
        "Wave",
        "MaxHP",
        "NetMulticast_OnSendHandleDamageInfos",
    ];
    if protocol_markers.iter().any(|marker| value.contains(marker)) {
        return 100 + length.min(100);
    }
    if value.starts_with("/Game/") {
        return 200 + length.min(100);
    }

    let structured_identifier = value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'/' | b'-')
    });
    let has_upper = value.bytes().any(|byte| byte.is_ascii_uppercase());
    let has_lower = value.bytes().any(|byte| byte.is_ascii_lowercase());
    let has_structure = value.contains('_') || value.contains('.') || value.contains("::");
    let bytes = value.as_bytes();
    let unreal_type_name = bytes.len() >= 2
        && matches!(bytes[0], b'A' | b'E' | b'F' | b'U')
        && bytes[1].is_ascii_uppercase()
        && has_upper
        && has_lower;
    if length >= 8
        && structured_identifier
        && (has_structure || unreal_type_name)
        && letters >= 5
        && punctuation * 4 <= length
    {
        return 20 + length.min(50);
    }
    0
}

fn length_prefixed_identifier_score(value: &str) -> usize {
    let length = value.len();
    if !(4..=96).contains(&length)
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'/' | b'-' | b' ')
        })
    {
        return 0;
    }
    let letters = value.bytes().filter(u8::is_ascii_alphabetic).count();
    let has_upper = value.bytes().any(|byte| byte.is_ascii_uppercase());
    let has_lower = value.bytes().any(|byte| byte.is_ascii_lowercase());
    if letters < 4 || !has_upper || !has_lower {
        return 0;
    }
    80 + length.min(80)
}

fn extract_length_prefixed_identifiers(data: &[u8]) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for offset in 0..data.len().saturating_sub(8) {
        let Some(length_bytes) = data.get(offset..offset + 4) else {
            continue;
        };
        let length = u32::from_le_bytes(length_bytes.try_into().unwrap()) as usize;
        if !(5..=97).contains(&length) {
            continue;
        }
        let Some(raw) = data.get(offset + 4..offset + 4 + length) else {
            continue;
        };
        let Some(value_bytes) = raw.strip_suffix(&[0]) else {
            continue;
        };
        let Ok(value) = std::str::from_utf8(value_bytes) else {
            continue;
        };
        let score = length_prefixed_identifier_score(value);
        if score > 0 && !found.iter().any(|(_, item)| item == value) {
            found.push((score, value.to_owned()));
        }
    }
    found
}

fn decode_payload_text(data: &[u8]) -> String {
    PayloadScan::new(data).decode_text()
}

fn is_padding_payload(data: &[u8]) -> bool {
    data.is_empty()
        || data
            .first()
            .is_some_and(|first| data.iter().all(|byte| byte == first))
}

fn should_keep_debug_packet(
    parsed_hits: usize,
    has_hp_updates: bool,
    has_targetish_or_runtime: bool,
    has_abyss_event: bool,
    packet_debug_mode: PacketDebugMode,
) -> bool {
    if matches!(packet_debug_mode, PacketDebugMode::FullPayload) {
        return true;
    }
    parsed_hits > 0 || has_hp_updates || has_targetish_or_runtime || has_abyss_event
}

fn should_fast_skip_payload(
    payload: &[u8],
    declared_ids: &[u32],
    has_hp_updates: bool,
    has_text_hint: bool,
) -> bool {
    if payload.len() > MAX_IGNORABLE_BINARY_PACKET_LEN {
        return false;
    }
    if payload.len() >= MIN_POSSIBLE_DAMAGE_PACKET_LEN {
        return false;
    }
    if has_hp_updates || !declared_ids.is_empty() {
        return false;
    }
    !has_text_hint
}

fn packet_target_path_rank(path: &str) -> usize {
    let lower = path.to_ascii_lowercase();
    let mut rank = path.len();
    if lower.starts_with("/game/") {
        rank += 10_000;
    }
    if lower.contains("/monster/") {
        rank += 2_000;
    }
    if lower.contains("_bp") {
        rank += 1_000;
    }
    rank
}

fn is_non_player_damage_effect(effect_name: Option<&str>) -> bool {
    let Some(effect_name) = effect_name else {
        return false;
    };
    let lower = effect_name.to_ascii_lowercase();
    lower.contains("killself") || lower.contains("self_destruct") || lower.contains("selfdestruct")
}

fn has_net_runtime_text_hint(decoded_text: &str) -> bool {
    if decoded_text == UNREADABLE_PROTOCOL_TEXT {
        return false;
    }
    let lower = decoded_text.to_ascii_lowercase();
    [
        "actorchannel",
        "packagemap",
        "netguid",
        "netrefhandle",
        "iris",
        "clientsetreplicatedtargetdata",
        "clientupdatetargetextradamageinfos",
        "clientserverabilityactorsetcurrenthp",
        "fclientrepfightdata",
        "fclientrepextradamageinfo",
        "gameplayeffect",
        "gameplaycue",
        "serverreceivegameplayeventtoactor",
        "netmulticast_onsendhandledamageinfos",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn shannon_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0_usize; 256];
    for byte in data {
        counts[*byte as usize] += 1;
    }
    let length = data.len() as f64;
    counts
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let probability = count as f64 / length;
            -probability * probability.log2()
        })
        .sum()
}

fn binary_payload_diagnostic(
    payload: &[u8],
    direction: &str,
    decoded_text: &str,
    evidence: &[(u32, u8, usize)],
) -> Option<String> {
    if direction != "S2C" || decoded_text != UNREADABLE_PROTOCOL_TEXT {
        return None;
    }
    if !evidence.is_empty() {
        let mut anchors = evidence
            .iter()
            .map(|(id, shift, offset)| format!("{id}@bit{}", offset * 8 + *shift as usize))
            .collect::<Vec<_>>();
        anchors.sort();
        anchors.dedup();
        let alignments = evidence
            .iter()
            .map(|(_, shift, _)| *shift)
            .collect::<HashSet<_>>()
            .len();
        return Some(format!(
            "detected {} character anchors across {} bit alignments: {}",
            anchors.len(),
            alignments,
            anchors.join(", ")
        ));
    }
    if payload.len() < 300 || is_padding_payload(payload) {
        return None;
    }
    let zero_ratio =
        payload.iter().filter(|byte| **byte == 0).count() as f64 / payload.len() as f64;
    let entropy = shannon_entropy(payload);
    if zero_ratio < 0.20 || entropy > 5.5 {
        return None;
    }
    Some(format!(
        "candidate packed replication payload: zero_ratio={:.1}%, entropy={:.2} bit/byte",
        zero_ratio * 100.0,
        entropy
    ))
}

fn append_packet_note(note: &mut String, diagnostic: Option<String>) {
    let Some(diagnostic) = diagnostic else {
        return;
    };
    if note.contains(&diagnostic) {
        return;
    }
    if !note.is_empty() {
        note.push_str("; ");
    }
    note.push_str(&diagnostic);
}

fn short_hex(bytes: &[u8]) -> String {
    let hex = hex::encode(bytes);
    if hex.len() <= 16 {
        hex
    } else {
        format!("{}…{}", &hex[..12], &hex[hex.len() - 4..])
    }
}

fn attach_runtime_alias_context(
    hit: &mut Hit,
    net_identity_candidates: &[crate::net_identity::NetIdentityCandidate],
    net_runtime_events: &[NetRuntimeEvent],
) {
    const HIT_ALIAS_WINDOW_BYTES: usize = 128;
    for candidate in net_identity_candidates
        .iter()
        .filter(|candidate| candidate.bit_shift == hit.bit_shift)
        .filter(|candidate| {
            candidate.byte_offset.abs_diff(hit.byte_offset) <= HIT_ALIAS_WINDOW_BYTES
        })
        .take(4)
    {
        hit.target_context
            .push(format!("{}={}", candidate.kind.label(), candidate.handle));
    }
    for event in net_runtime_events
        .iter()
        .filter(|event| event.bit_shift == hit.bit_shift)
        .filter(|event| event.byte_offset.abs_diff(hit.byte_offset) <= HIT_ALIAS_WINDOW_BYTES)
        .flat_map(|event| event.aliases.iter())
        .take(4)
    {
        push_unique_context(
            &mut hit.target_context,
            format!("{}={}", event.kind.label(), event.value),
        );
    }
}

fn current_hp_target_token(prefix: &[u8; 16]) -> Option<[u8; 4]> {
    let token = [prefix[0], prefix[7], prefix[9], prefix[10]];
    let distinct = token.iter().copied().collect::<HashSet<_>>().len();
    (token.iter().any(|byte| *byte != 0) && distinct >= 2).then_some(token)
}

fn parse_abyss_stage_id(value: &str) -> Option<(u32, u32, AbyssHalf)> {
    let parts: Vec<_> = value.split('_').collect();
    if parts.len() < 4 || parts.first().copied() != Some("Abyss") {
        return None;
    }
    let cycle = parts.get(parts.len() - 3)?.parse().ok()?;
    let floor = parts.get(parts.len() - 2)?.parse().ok()?;
    let half = match *parts.last()? {
        "0" => AbyssHalf::First,
        "1" => AbyssHalf::Second,
        _ => return None,
    };
    Some((cycle, floor, half))
}

fn abyss_events_from_text(timestamp: f64, decoded_text: &str) -> Vec<AbyssEvent> {
    let mut events = Vec::new();
    let is_restart = decoded_text.contains("Abyss_Battle_Born");
    if is_restart {
        events.push(AbyssEvent::RestartDetected { timestamp });
    }
    let is_success = decoded_text.contains("ConditionState_Success")
        && decoded_text.contains("FAbyssGamePlayData");
    let mut explicit_stage = None;
    if !is_success {
        for value in decoded_text.lines() {
            if let Some(stage) = parse_abyss_stage_id(value) {
                explicit_stage = Some(stage);
            }
        }
    }
    if let Some((cycle, floor, half)) = explicit_stage {
        events.push(AbyssEvent::Stage {
            timestamp,
            cycle: Some(cycle),
            floor: Some(floor),
            half,
        });
    } else if !is_success
        && !is_restart
        && decoded_text.contains("FAbyssGamePlayData")
        && !decoded_text.contains("AbyssClone")
    {
        let first = decoded_text.contains("EAbyssFightStage::FirstHalf");
        let second = decoded_text.contains("EAbyssFightStage::SecondHalf");
        if first ^ second {
            events.push(AbyssEvent::Stage {
                timestamp,
                cycle: None,
                floor: None,
                half: if first {
                    AbyssHalf::First
                } else {
                    AbyssHalf::Second
                },
            });
        }
    }
    if is_success {
        events.push(AbyssEvent::Success { timestamp });
    }
    if decoded_text.contains("Abyss_Station_LeaveClone") {
        events.push(AbyssEvent::Exit { timestamp });
    }
    events
}

fn send_packet_events(sender: &Sender<EngineEvent>, packet: PacketDebug) {
    for event in abyss_events_from_text(packet.timestamp, &packet.decoded_text) {
        let _ = sender.send(EngineEvent::Abyss(event));
    }
    let _ = sender.send(EngineEvent::Packet(packet));
}

const INFERRED_FOLLOW_UP_CHAR_ID: u32 = u32::MAX;
const NANALLY_MELEE1_GAMEPLAY_EFFECT_INDEX: u32 = 241;
const MAX_DISPLAY_DAMAGE_CORRECTION_RATIO: f64 = 0.03;
const MAX_PENDING_FOLLOW_UP_HITS: usize = 256;
const MAX_RECENT_TARGET_HITS: usize = 2048;
const TARGET_BACKFILL_WINDOW_SECONDS: f64 = 90.0;

#[derive(Clone)]
struct PendingHit {
    hit: Hit,
    gameplay_effect_index: Option<u32>,
}

#[derive(Default)]
struct FollowUpDamageTracker {
    last_server_hp: Option<f64>,
    last_hit_timestamp: Option<f64>,
    target_max_hp: Option<f64>,
    pending_hits: VecDeque<PendingHit>,
    team_attributes: HashSet<String>,
    character_attributes: HashMap<u32, String>,
}

impl FollowUpDamageTracker {
    fn reset_battle(&mut self) {
        self.pending_hits.clear();
        self.team_attributes.clear();
        self.character_attributes.clear();
    }

    fn observe_characters(
        &mut self,
        character_ids: impl IntoIterator<Item = u32>,
        characters: &HashMap<u32, CharacterInfo>,
    ) {
        for character_id in character_ids {
            let Some(attribute) = characters
                .get(&character_id)
                .and_then(|character| character.attribute.as_deref())
            else {
                continue;
            };
            self.team_attributes.insert(attribute.to_owned());
            self.character_attributes
                .insert(character_id, attribute.to_owned());
        }
    }

    fn observe_hit(
        &mut self,
        hit: &Hit,
        gameplay_effect_index: Option<u32>,
        characters: &HashMap<u32, CharacterInfo>,
    ) {
        if hit.direction == "incoming"
            || hit.char_id == 0
            || hit.target_max_hp <= 500_000.0
            || hit.target_hp_before <= 0.0
        {
            return;
        }
        let new_full_health_battle = self.last_hit_timestamp.is_some_and(|last_timestamp| {
            hit.timestamp - last_timestamp > 10.0 && hit.target_hp_before >= hit.target_max_hp * 0.9
        });
        let changed_target_max_hp = self
            .target_max_hp
            .is_some_and(|maximum| (maximum - hit.target_max_hp).abs() > 1.0);
        if new_full_health_battle || changed_target_max_hp {
            self.reset_battle();
            self.last_server_hp = None;
        }
        self.last_hit_timestamp = Some(hit.timestamp);
        self.target_max_hp = Some(hit.target_max_hp);
        self.observe_characters([hit.char_id], characters);
        if self
            .pending_hits
            .back()
            .is_some_and(|previous| hit.timestamp - previous.hit.timestamp > 1.0)
        {
            self.pending_hits.clear();
        }
        self.pending_hits.push_back(PendingHit {
            hit: hit.clone(),
            gameplay_effect_index,
        });
        while self.pending_hits.len() > MAX_PENDING_FOLLOW_UP_HITS {
            self.pending_hits.pop_front();
        }
    }

    fn observe_server_hp(&mut self, timestamp: f64, current_hp: f64) -> Option<Hit> {
        self.pending_hits
            .retain(|pending| timestamp - pending.hit.timestamp <= 1.0);
        let previous_hp = self.last_server_hp.or_else(|| {
            self.pending_hits
                .front()
                .map(|pending| pending.hit.target_hp_before)
        });
        self.last_server_hp = Some(current_hp);
        let previous_hp = previous_hp?;
        if current_hp >= previous_hp || self.pending_hits.is_empty() {
            if current_hp > previous_hp {
                let reset_threshold = self.target_max_hp.unwrap_or(current_hp) * 0.25;
                if current_hp - previous_hp >= reset_threshold {
                    self.reset_battle();
                } else {
                    self.pending_hits.clear();
                }
            }
            return None;
        }

        let actual_damage = previous_hp - current_hp;
        let pending = self.pending_hits.pop_front()?;
        let source = pending.hit;
        let residual_damage = actual_damage - source.damage;
        let inferred_damage =
            corrected_follow_up_damage(actual_damage, source.damage, pending.gameplay_effect_index)
                .unwrap_or(residual_damage);
        let inferred_ratio = if source.damage > 0.0 {
            residual_damage / source.damage
        } else {
            0.0
        };
        let corrected = (inferred_damage - residual_damage).abs() > 0.5;
        if inferred_damage < 1.0 || (!corrected && !(0.18..=0.26).contains(&inferred_ratio)) {
            return None;
        }
        let has_required_team_attributes =
            self.team_attributes.contains("灵") && self.team_attributes.contains("咒");
        let source_attribute = self.character_attributes.get(&source.char_id)?;
        if !has_required_team_attributes || !matches!(source_attribute.as_str(), "灵" | "咒") {
            return None;
        }
        Some(Hit {
            timestamp,
            char_id: INFERRED_FOLLOW_UP_CHAR_ID,
            char_name: "覆纹伤害".to_owned(),
            char_known: false,
            damage: inferred_damage,
            byte_offset: 0,
            bit_shift: 0,
            char_source: "boss_hp_residual".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: current_hp + inferred_damage,
            target_hp_after: current_hp,
            target_max_hp: source.target_max_hp,
            target_hp_percent: if source.target_max_hp > 0.0 {
                current_hp / source.target_max_hp * 100.0
            } else {
                0.0
            },
            target_id: None,
            target_name: None,
            target_context: Vec::new(),
            gameplay_effect_index: None,
            gameplay_effect_name: None,
            ability_name: None,
            damage_name: Some("覆纹追加攻击".to_owned()),
            attack_type: Some("覆纹".to_owned()),
        })
    }
}

fn corrected_follow_up_damage(
    actual_damage: f64,
    packet_damage: f64,
    gameplay_effect_index: Option<u32>,
) -> Option<f64> {
    if gameplay_effect_index != Some(NANALLY_MELEE1_GAMEPLAY_EFFECT_INDEX) {
        return None;
    }
    let total = actual_damage.round();
    if (actual_damage - total).abs() > 0.01 || total < 1.0 {
        return None;
    }
    let total = total as u64;
    let minimum_base = total.saturating_mul(5) / 6;
    for displayed_base in minimum_base.saturating_sub(2)..=minimum_base.saturating_add(3) {
        let follow_up = displayed_base / 5;
        if displayed_base + follow_up != total {
            continue;
        }
        let correction_ratio =
            (displayed_base as f64 - packet_damage).abs() / displayed_base as f64;
        if correction_ratio <= MAX_DISPLAY_DAMAGE_CORRECTION_RATIO {
            return Some(follow_up as f64);
        }
    }
    None
}

struct PacketDecoder {
    session_characters: HashMap<(Ipv4Addr, u16, Ipv4Addr, u16), u32>,
    client_endpoints: HashSet<(Ipv4Addr, u16)>,
    gameplay_effect_names: HashMap<u32, String>,
    gameplay_effect_skills: HashMap<String, GameplayEffectSkill>,
    wooden_damage_names: HashMap<String, String>,
    object_state: ObjectStateStore,
    target_instances: TargetInstanceStore,
    target_tracks: TargetTrackStore,
    resource_index: ResourceIndex,
    target_resolver: TargetResolver,
    recent_target_hits: VecDeque<RecentTargetHit>,
    target_handle_aliases: HashMap<String, HandleTargetAlias>,
    recent_dead_target_handles: HashMap<String, f64>,
    follow_up_damage: FollowUpDamageTracker,
    character_declarations: HashMap<u32, f64>,
    resource_warnings: Vec<String>,
    packet_debug_mode: PacketDebugMode,
}

#[derive(Clone, Debug)]
struct RecentTargetHit {
    hit: Hit,
    summary: TargetResolutionSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HandleTargetAlias {
    target_path: String,
    target_name: String,
    source: String,
    first_seen_at: u64,
    last_seen_at: u64,
}

impl Default for PacketDecoder {
    fn default() -> Self {
        let mut resource_warnings = Vec::new();
        let gameplay_effect_names = load_resource(
            GAMEPLAY_EFFECT_MAPPING_PATH,
            &mut resource_warnings,
            load_gameplay_effect_mapping,
        );
        let gameplay_effect_skills = load_resource(
            SKILL_DAMAGE_DATA_PATH,
            &mut resource_warnings,
            load_gameplay_effect_skills,
        );
        let wooden_damage_names = load_resource(
            WOODEN_DAMAGE_DESCRIPTIONS_PATH,
            &mut resource_warnings,
            load_wooden_damage_names,
        );

        Self {
            session_characters: HashMap::new(),
            client_endpoints: HashSet::new(),
            gameplay_effect_names,
            gameplay_effect_skills,
            wooden_damage_names,
            object_state: ObjectStateStore::default(),
            target_instances: TargetInstanceStore::default(),
            target_tracks: TargetTrackStore::default(),
            resource_index: ResourceIndex::load_default_with_warnings(&mut resource_warnings),
            target_resolver: TargetResolver,
            recent_target_hits: VecDeque::new(),
            target_handle_aliases: HashMap::new(),
            recent_dead_target_handles: HashMap::new(),
            follow_up_damage: FollowUpDamageTracker::default(),
            character_declarations: HashMap::new(),
            resource_warnings,
            packet_debug_mode: PacketDebugMode::Off,
        }
    }
}

impl PacketDecoder {
    fn with_packet_debug_mode(packet_debug_mode: PacketDebugMode) -> Self {
        Self {
            packet_debug_mode,
            ..Self::default()
        }
    }
}

fn target_update_from_hit(hit: &Hit, summary: &TargetResolutionSummary) -> HitTargetUpdate {
    HitTargetUpdate {
        hit_uid: stable_hit_uid(hit),
        timestamp: hit.timestamp,
        char_id: hit.char_id,
        damage: hit.damage,
        byte_offset: hit.byte_offset,
        bit_shift: hit.bit_shift,
        target_id: hit.target_id.clone(),
        target_name: hit.target_name.clone(),
        target_context: hit.target_context.clone(),
        target_score: summary.score,
        target_confidence: summary.confidence.as_str().to_owned(),
        old_target_id: None,
        update_reason: target_context_value(&hit.target_context, "target_name_resolution")
            .map(str::to_owned),
        update_strength: Some(summary.confidence.as_str().to_owned()),
        target_generation: hit
            .target_id
            .as_deref()
            .and_then(|target_id| target_id.rsplit_once('#').map(|(_, generation)| generation))
            .map(str::to_owned),
    }
}

fn target_lock_context_for_packet(
    path_candidates: &[PathCandidate],
    current_hp_updates: &[ParsedCurrentHpUpdate],
    boss_hp_updates: &[ParsedBossHpUpdate],
    resources: &ResourceIndex,
) -> TrackPacketContext {
    let mut canonical_target_paths = HashSet::new();
    let mut canonical_target_names = HashSet::new();
    for candidate in path_candidates {
        if is_ignored_non_target_path(&candidate.value) || !is_targetish_path(&candidate.value) {
            continue;
        }
        let canonical_path = resources
            .canonical_target_path_for_path(&candidate.value)
            .unwrap_or_else(|| candidate.value.clone());
        let Some(name) = resources
            .resolved_name_for_path(&candidate.value)
            .or_else(|| resources.resolved_name_for_path(&canonical_path))
        else {
            continue;
        };
        canonical_target_paths.insert(canonical_path);
        canonical_target_names.insert(name);
    }
    let mut hp_handles = HashSet::new();
    for update in boss_hp_updates {
        hp_handles.insert(format!(
            "boss_hp_guid:{}",
            hex::encode(update.target_handle)
        ));
    }
    for update in current_hp_updates {
        if let Some(token) = current_hp_target_token(&update.target_hint) {
            hp_handles.insert(format!("current_hp_token:{}", hex::encode(token)));
        }
    }
    let has_multiple_canonical_targets =
        canonical_target_paths.len() > 1 || canonical_target_names.len() > 1;
    TrackPacketContext {
        canonical_target_paths,
        canonical_target_names,
        local_canonical_target_paths: HashSet::new(),
        local_canonical_target_names: HashSet::new(),
        hp_handle_keys: hp_handles,
        has_multiple_canonical_targets,
    }
}

fn enrich_track_context_from_hit(
    context: &mut TrackPacketContext,
    hit: &Hit,
    resources: &ResourceIndex,
) {
    for entry in &hit.target_context {
        let Some(path) = entry.strip_prefix("reason=near_object_path:") else {
            continue;
        };
        if is_ignored_non_target_path(path) {
            continue;
        }
        let canonical_path = resources
            .canonical_target_path_for_path(path)
            .unwrap_or_else(|| path.to_owned());
        let Some(name) = resources
            .resolved_name_for_path(path)
            .or_else(|| resources.resolved_name_for_path(&canonical_path))
        else {
            continue;
        };
        context
            .local_canonical_target_paths
            .insert(canonical_path.clone());
        context.local_canonical_target_names.insert(name.clone());
        context.canonical_target_paths.insert(canonical_path);
        context.canonical_target_names.insert(name);
    }
    context.has_multiple_canonical_targets =
        context.canonical_target_paths.len() > 1 || context.canonical_target_names.len() > 1;
}

fn load_resource<T>(
    relative_path: &str,
    warnings: &mut Vec<String>,
    loader: impl FnOnce(&Path) -> anyhow::Result<T>,
) -> T
where
    T: Default,
{
    let path = Path::new(relative_path);
    let Some(path) = find_data_file(path) else {
        warnings.push(format!("missing resource {relative_path}"));
        return T::default();
    };
    match loader(&path) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(format!("{}: {error}", path.display()));
            T::default()
        }
    }
}

impl PacketDecoder {
    fn resource_warning(&self) -> Option<String> {
        (!self.resource_warnings.is_empty()).then(|| self.resource_warnings.join("; "))
    }

    fn remember_target_hit(&mut self, hit: &Hit, summary: TargetResolutionSummary) {
        if hit.direction == "incoming" {
            return;
        }
        self.register_target_handle_alias_for_hit(hit, &summary);
        self.recent_target_hits.push_back(RecentTargetHit {
            hit: hit.clone(),
            summary,
        });
        while self.recent_target_hits.len() > MAX_RECENT_TARGET_HITS {
            self.recent_target_hits.pop_front();
        }
    }

    fn backfill_recent_targets(&mut self, timestamp: f64, sender: &Sender<EngineEvent>) {
        self.recent_target_hits
            .retain(|recent| timestamp - recent.hit.timestamp <= TARGET_BACKFILL_WINDOW_SECONDS);
        let discovered_aliases = self
            .recent_target_hits
            .iter()
            .filter(|recent| can_register_target_handle_alias_from_hit(&recent.hit))
            .flat_map(|recent| {
                let old_alias_keys = target_alias_lookup_keys(
                    recent
                        .hit
                        .target_id
                        .as_ref()
                        .or(recent.summary.target_id.as_ref()),
                    &recent.hit.target_context,
                );
                let mut resolved = recent.hit.clone();
                resolved.target_id = None;
                resolved.target_name = None;
                resolved.target_context.clear();
                let resolved_summary = self.target_resolver.apply_to_hit_with_summary(
                    &mut resolved,
                    &self.object_state,
                    &self.target_instances,
                    &[],
                    &self.resource_index,
                );
                let mut aliases =
                    target_handle_aliases_from_summary(&resolved_summary, recent.hit.timestamp);
                if let Some(alias) =
                    handle_target_alias_from_summary(&resolved_summary, recent.hit.timestamp)
                {
                    aliases.extend(old_alias_keys.into_iter().map(|key| (key, alias.clone())));
                }
                aliases
            })
            .collect::<Vec<_>>();
        for (target_id, alias) in discovered_aliases {
            merge_target_handle_alias(&mut self.target_handle_aliases, target_id, alias);
        }
        let target_handle_aliases = self.target_handle_aliases.clone();
        for recent in &mut self.recent_target_hits {
            if apply_handle_alias_backfill(recent, &target_handle_aliases) {
                let _ = sender.send(EngineEvent::HitTargetUpdate(target_update_from_hit(
                    &recent.hit,
                    &recent.summary,
                )));
                continue;
            }
            let old_alias_keys = target_alias_lookup_keys(
                recent
                    .hit
                    .target_id
                    .as_ref()
                    .or(recent.summary.target_id.as_ref()),
                &recent.hit.target_context,
            );
            let mut resolved = recent.hit.clone();
            resolved.target_id = None;
            resolved.target_name = None;
            resolved.target_context.clear();
            let resolved_summary = self.target_resolver.apply_to_hit_with_summary(
                &mut resolved,
                &self.object_state,
                &self.target_instances,
                &[],
                &self.resource_index,
            );
            if should_apply_target_update(&recent.summary, &resolved_summary) {
                let mut resolved_summary = resolved_summary;
                if should_preserve_existing_target_id_for_name_update(
                    &recent.summary,
                    &resolved_summary,
                ) {
                    resolved.target_id = recent.hit.target_id.clone();
                    resolved.target_context.push(format!(
                        "target_id_preserved={}",
                        recent.hit.target_id.as_deref().unwrap_or_default()
                    ));
                    resolved_summary.target_id = recent.summary.target_id.clone();
                    resolved_summary.score = recent.summary.score;
                    resolved_summary.confidence = recent.summary.confidence;
                    resolved_summary.direct_hp_evidence = recent.summary.direct_hp_evidence;
                    resolved_summary.target_context = resolved.target_context.clone();
                }
                recent.hit = resolved;
                if can_register_target_handle_alias_from_hit(&recent.hit) {
                    for (target_id, alias) in
                        target_handle_aliases_from_summary(&resolved_summary, recent.hit.timestamp)
                    {
                        merge_target_handle_alias(
                            &mut self.target_handle_aliases,
                            target_id,
                            alias,
                        );
                    }
                    if let Some(alias) =
                        handle_target_alias_from_summary(&resolved_summary, recent.hit.timestamp)
                    {
                        for target_id in old_alias_keys {
                            merge_target_handle_alias(
                                &mut self.target_handle_aliases,
                                target_id,
                                alias.clone(),
                            );
                        }
                    }
                }
                recent.summary = resolved_summary;
                let _ = sender.send(EngineEvent::HitTargetUpdate(target_update_from_hit(
                    &recent.hit,
                    &recent.summary,
                )));
            }
        }
    }

    fn register_target_handle_alias(&mut self, summary: &TargetResolutionSummary, timestamp: f64) {
        for (target_id, alias) in target_handle_aliases_from_summary(summary, timestamp) {
            merge_target_handle_alias(&mut self.target_handle_aliases, target_id, alias);
        }
    }

    fn register_target_handle_alias_for_hit(
        &mut self,
        hit: &Hit,
        summary: &TargetResolutionSummary,
    ) {
        if can_register_target_handle_alias_from_hit(hit) {
            self.register_target_handle_alias(summary, hit.timestamp);
        }
    }

    fn apply_target_track_attribution(
        &mut self,
        hit: &mut Hit,
        summary: &mut TargetResolutionSummary,
        mut context: TrackPacketContext,
    ) {
        if hit.direction == "incoming" {
            return;
        }
        enrich_track_context_from_hit(&mut context, hit, &self.resource_index);
        self.target_tracks
            .attribute_damage_hit(hit, summary, context);
    }

    fn register_target_instance_alias_keys(
        &mut self,
        keys: impl IntoIterator<Item = String>,
        instance_id: &str,
        timestamp: f64,
    ) {
        let Some(instance) = self.target_instances.instance(instance_id) else {
            return;
        };
        if instance.target_name.trim().is_empty()
            || is_ignored_non_target_path(&instance.canonical_path)
        {
            return;
        }
        let alias = HandleTargetAlias {
            target_path: instance.canonical_path.clone(),
            target_name: instance.target_name.clone(),
            source: "runtime_instance_handle_alias".to_owned(),
            first_seen_at: timestamp.to_bits(),
            last_seen_at: timestamp.to_bits(),
        };
        for key in keys {
            merge_target_handle_alias(&mut self.target_handle_aliases, key, alias.clone());
        }
    }

    fn remove_target_handle_alias_keys(&mut self, keys: impl IntoIterator<Item = String>) {
        for key in keys {
            for alias_key in equivalent_target_alias_keys(&key) {
                self.target_handle_aliases.remove(&alias_key);
            }
        }
    }

    fn remember_dead_target_handle(&mut self, key: String, timestamp: f64) {
        for key in equivalent_target_alias_keys(&key) {
            self.recent_dead_target_handles.insert(key, timestamp);
        }
    }

    fn target_handle_recently_dead(&self, key: &str, timestamp: f64) -> bool {
        equivalent_target_alias_keys(key).into_iter().any(|key| {
            self.recent_dead_target_handles
                .get(&key)
                .is_some_and(|dead_at| {
                    timestamp >= *dead_at
                        && timestamp - *dead_at <= DEAD_TARGET_HANDLE_REUSE_WINDOW_SECONDS
                })
        })
    }

    fn cleanup_recent_dead_target_handles(&mut self, timestamp: f64) {
        self.recent_dead_target_handles.retain(|_, dead_at| {
            timestamp < *dead_at || timestamp - *dead_at <= DEAD_TARGET_HANDLE_REUSE_WINDOW_SECONDS
        });
    }

    fn clear_reused_current_hp_handle(&mut self, timestamp: f64, token: &str, evidence: &str) {
        let handle = format!("currenthp:{token}");
        self.object_state.clear_handle_identity(
            timestamp,
            ObjectHandleKind::NetRefHandleCandidate,
            &handle,
            evidence.to_owned(),
        );
        self.remove_target_handle_alias_keys([
            format!("current_hp_token:{token}"),
            format!("NetRefHandleCandidate:currenthp:{token}"),
        ]);
        let alias = TargetAlias::new(TargetAliasKind::CurrentHpToken, token.to_owned());
        let _ = self.target_instances.close_alias(timestamp, &alias, true);
    }

    fn clear_reused_boss_hp_handle(&mut self, timestamp: f64, handle: &str, evidence: &str) {
        self.object_state.clear_handle_identity(
            timestamp,
            ObjectHandleKind::AttributeGuid,
            handle,
            evidence.to_owned(),
        );
        self.remove_target_handle_alias_keys([
            format!("boss_hp_guid:{handle}"),
            format!("AttributeGuid:{handle}"),
        ]);
        let alias = TargetAlias::new(TargetAliasKind::BossHpGuid, handle.to_owned());
        let _ = self.target_instances.close_alias(timestamp, &alias, true);
    }

    fn clear_dead_target_handles_from_hit(&mut self, hit: &mut Hit) {
        if hit.direction == "incoming" || hit.target_hp_after > 1.0 {
            return;
        }
        let mut current_hp_tokens = HashSet::new();
        let mut boss_hp_handles = HashSet::new();
        for key in target_alias_lookup_keys(hit.target_id.as_ref(), &hit.target_context) {
            if let Some(token) = key
                .strip_prefix("NetRefHandleCandidate:currenthp:")
                .or_else(|| key.strip_prefix("current_hp_token:"))
            {
                current_hp_tokens.insert(token.to_owned());
            } else if let Some(handle) = key
                .strip_prefix("AttributeGuid:")
                .or_else(|| key.strip_prefix("boss_hp_guid:"))
            {
                boss_hp_handles.insert(handle.to_owned());
            }
        }
        for token in target_context_values(&hit.target_context, "current_hp_token") {
            current_hp_tokens.insert(token.to_owned());
        }
        for value in target_context_values(&hit.target_context, "target_handle_candidate") {
            if let Some(token) = value.strip_prefix("NetRefHandleCandidate:currenthp:") {
                current_hp_tokens.insert(token.to_owned());
            }
        }
        for handle in target_context_values(&hit.target_context, "boss_hp_guid") {
            boss_hp_handles.insert(handle.to_owned());
        }
        for token in current_hp_tokens {
            push_unique_context(
                &mut hit.target_context,
                format!("dead_current_hp_token_cleared={token}"),
            );
            self.remember_dead_target_handle(
                format!("NetRefHandleCandidate:currenthp:{token}"),
                hit.timestamp,
            );
            self.clear_reused_current_hp_handle(hit.timestamp, &token, "damage_hit_target_dead");
        }
        for handle in boss_hp_handles {
            push_unique_context(
                &mut hit.target_context,
                format!("dead_boss_hp_guid_cleared={handle}"),
            );
            self.remember_dead_target_handle(format!("AttributeGuid:{handle}"), hit.timestamp);
            self.clear_reused_boss_hp_handle(hit.timestamp, &handle, "damage_hit_target_dead");
        }
    }

    fn suppress_recently_dead_target_on_hit(
        &self,
        hit: &mut Hit,
        summary: &mut TargetResolutionSummary,
    ) -> bool {
        if hit.direction == "incoming" || hit.target_hp_after <= 1.0 {
            return false;
        }
        let stale_alias_handle = target_alias_lookup_keys(
            hit.target_id.as_ref().or(summary.target_id.as_ref()),
            &hit.target_context,
        )
        .into_iter()
        .filter(|key| is_hp_alias_key(key))
        .any(|key| self.target_handle_recently_dead(&key, hit.timestamp));
        let stale_current_hp_token = target_context_values(&hit.target_context, "current_hp_token")
            .any(|token| {
                self.target_handle_recently_dead(
                    &format!("NetRefHandleCandidate:currenthp:{token}"),
                    hit.timestamp,
                )
            });
        let stale_boss_hp_guid =
            target_context_values(&hit.target_context, "boss_hp_guid").any(|handle| {
                self.target_handle_recently_dead(&format!("AttributeGuid:{handle}"), hit.timestamp)
            });
        let stale_recent_hit = self.recent_target_hits.iter().any(|recent| {
            recent.hit.target_hp_after <= 1.0
                && hit.timestamp >= recent.hit.timestamp
                && hit.timestamp - recent.hit.timestamp <= DEAD_TARGET_HANDLE_REUSE_WINDOW_SECONDS
                && hits_share_hp_target_handle(hit, &recent.hit)
        });
        let stale_handle =
            stale_alias_handle || stale_current_hp_token || stale_boss_hp_guid || stale_recent_hit;
        if !stale_handle {
            return false;
        }
        suppress_hit_target_as_recent_death(hit, summary);
        true
    }

    fn apply_hp_updates_to_state(
        &mut self,
        timestamp: f64,
        current_hp_updates: &[ParsedCurrentHpUpdate],
        boss_hp_updates: &[ParsedBossHpUpdate],
    ) {
        let mut current_hp_reuse = HashMap::<String, (bool, bool)>::new();
        for update in current_hp_updates {
            let Some(target_token) = current_hp_target_token(&update.target_hint) else {
                continue;
            };
            let entry = current_hp_reuse
                .entry(hex::encode(target_token))
                .or_insert((false, false));
            entry.0 |= update.current_hp <= 1.0;
            entry.1 |= update.current_hp > 1.0;
        }
        let mut boss_hp_reuse = HashMap::<String, (bool, bool)>::new();
        for update in boss_hp_updates {
            let entry = boss_hp_reuse
                .entry(hex::encode(update.target_handle))
                .or_insert((false, false));
            entry.0 |= update.current_hp <= 1.0;
            entry.1 |= update.current_hp > 1.0;
        }

        for update in current_hp_updates {
            let Some(target_token) = current_hp_target_token(&update.target_hint) else {
                continue;
            };
            let token = hex::encode(target_token);
            let dead_key = format!("NetRefHandleCandidate:currenthp:{token}");
            let live_after_recent_death =
                update.current_hp > 1.0 && self.target_handle_recently_dead(&dead_key, timestamp);
            self.object_state.observe_net_target_hp_update(
                timestamp,
                "currenthp",
                &target_token,
                update.current_hp as f64,
                None,
                format!(
                    "current_hp:{}={:.0}@{}:{}",
                    short_hex(&target_token),
                    update.current_hp,
                    update.byte_offset,
                    update.bit_shift
                ),
            );
            if let Some(instance_id) = self.target_instances.observe_current_hp_token(
                timestamp,
                &target_token,
                update.current_hp as f64,
                format!(
                    "current_hp:{}={:.0}@{}:{}",
                    short_hex(&target_token),
                    update.current_hp,
                    update.byte_offset,
                    update.bit_shift
                ),
            ) {
                if update.current_hp <= 1.0 {
                    self.remember_dead_target_handle(dead_key, timestamp);
                    self.remove_target_handle_alias_keys([
                        format!("current_hp_token:{token}"),
                        format!("NetRefHandleCandidate:currenthp:{token}"),
                    ]);
                } else {
                    self.register_target_instance_alias_keys(
                        [
                            format!("current_hp_token:{token}"),
                            format!("NetRefHandleCandidate:currenthp:{token}"),
                        ],
                        &instance_id,
                        timestamp,
                    );
                }
            } else if update.current_hp <= 1.0 {
                self.remember_dead_target_handle(dead_key, timestamp);
                self.remove_target_handle_alias_keys([
                    format!("current_hp_token:{token}"),
                    format!("NetRefHandleCandidate:currenthp:{token}"),
                ]);
            }
            if live_after_recent_death {
                self.clear_reused_current_hp_handle(
                    timestamp,
                    &token,
                    "hp_handle_live_after_recent_death",
                );
            }
        }
        for update in boss_hp_updates {
            let handle = hex::encode(update.target_handle);
            let dead_key = format!("AttributeGuid:{handle}");
            let live_after_recent_death =
                update.current_hp > 1.0 && self.target_handle_recently_dead(&dead_key, timestamp);
            self.object_state.observe_hp_guid_update(
                timestamp,
                update.target_handle,
                update.current_hp as f64,
                None,
                format!(
                    "boss_hp:{}={:.0}@{}:{}",
                    hex::encode(update.target_handle),
                    update.current_hp,
                    update.byte_offset,
                    update.bit_shift
                ),
            );
            if let Some(instance_id) = self.target_instances.observe_boss_hp_guid(
                timestamp,
                update.target_handle,
                update.current_hp as f64,
                format!(
                    "boss_hp:{}={:.0}@{}:{}",
                    hex::encode(update.target_handle),
                    update.current_hp,
                    update.byte_offset,
                    update.bit_shift
                ),
            ) {
                if update.current_hp <= 1.0 {
                    self.remember_dead_target_handle(dead_key, timestamp);
                    self.remove_target_handle_alias_keys([
                        format!("boss_hp_guid:{handle}"),
                        format!("AttributeGuid:{handle}"),
                    ]);
                } else {
                    self.register_target_instance_alias_keys(
                        [
                            format!("boss_hp_guid:{handle}"),
                            format!("AttributeGuid:{handle}"),
                        ],
                        &instance_id,
                        timestamp,
                    );
                }
            } else if update.current_hp <= 1.0 {
                self.remember_dead_target_handle(dead_key, timestamp);
                self.remove_target_handle_alias_keys([
                    format!("boss_hp_guid:{handle}"),
                    format!("AttributeGuid:{handle}"),
                ]);
            }
            if live_after_recent_death {
                self.clear_reused_boss_hp_handle(
                    timestamp,
                    &handle,
                    "hp_handle_live_after_recent_death",
                );
            }
        }
        for (token, (saw_dead, saw_live)) in current_hp_reuse {
            if !(saw_dead && saw_live) {
                continue;
            }
            self.clear_reused_current_hp_handle(timestamp, &token, "hp_handle_reused_after_death");
        }
        for (handle, (saw_dead, saw_live)) in boss_hp_reuse {
            if !(saw_dead && saw_live) {
                continue;
            }
            self.clear_reused_boss_hp_handle(timestamp, &handle, "hp_handle_reused_after_death");
        }
        self.cleanup_recent_dead_target_handles(timestamp);
    }

    fn apply_runtime_mapping_events(
        &mut self,
        events: Vec<RuntimeMappingEvent>,
        sender: &Sender<EngineEvent>,
    ) {
        if events.is_empty() {
            return;
        }
        let mut applied = 0_usize;
        let mut latest_timestamp = 0.0_f64;
        for event in events {
            latest_timestamp = latest_timestamp.max(event.timestamp);
            if self.apply_runtime_mapping_event(event) {
                applied += 1;
            }
        }
        if applied > 0 {
            self.backfill_recent_targets(latest_timestamp, sender);
        }
    }

    fn apply_runtime_mapping_event(&mut self, event: RuntimeMappingEvent) -> bool {
        match event.action {
            RuntimeMappingAction::Map => self.apply_runtime_mapping_map(event),
            RuntimeMappingAction::Close | RuntimeMappingAction::Destroy => {
                let expire_instance = event.action == RuntimeMappingAction::Destroy;
                let mut applied = false;
                for alias in &event.aliases {
                    applied |= self
                        .target_instances
                        .close_alias(event.timestamp, alias, expire_instance)
                        .is_some();
                    self.remove_target_handle_alias_keys([alias.key()]);
                }
                applied
            }
        }
    }

    fn apply_runtime_mapping_map(&mut self, event: RuntimeMappingEvent) -> bool {
        let Some((canonical_path, target_name)) = self.runtime_mapping_target(&event) else {
            return false;
        };
        let aliases = event.aliases.clone();
        let instance_id = self.target_instances.observe_runtime_mapping(
            event.timestamp,
            canonical_path.clone(),
            target_name,
            aliases.clone(),
        );
        for alias in &aliases {
            self.observe_runtime_alias_in_object_state(event.timestamp, alias, &canonical_path);
        }
        self.register_target_instance_alias_keys(
            runtime_mapping_alias_keys(&aliases),
            &instance_id,
            event.timestamp,
        );
        true
    }

    fn runtime_mapping_target(&self, event: &RuntimeMappingEvent) -> Option<(String, String)> {
        let raw_path = event
            .object_path
            .as_deref()
            .or(event.class_path.as_deref())?;
        if is_ignored_non_target_path(raw_path) && event.target_name.is_none() {
            return None;
        }
        let canonical_path = self
            .resource_index
            .canonical_target_path_for_path(raw_path)
            .unwrap_or_else(|| raw_path.to_owned());
        let target_name = event
            .target_name
            .clone()
            .or_else(|| self.resource_index.resolved_name_for_path(raw_path))
            .or_else(|| self.resource_index.resolved_name_for_path(&canonical_path))
            .or_else(|| self.resource_index.display_name_for_path(&canonical_path))?;
        if target_name.trim().is_empty() {
            return None;
        }
        Some((canonical_path, target_name))
    }

    fn observe_runtime_alias_in_object_state(
        &mut self,
        timestamp: f64,
        alias: &TargetAlias,
        canonical_path: &str,
    ) {
        let Some(handle_kind) = object_handle_kind_for_runtime_alias(alias.kind) else {
            return;
        };
        self.object_state.observe_path_handle_candidate(
            timestamp,
            handle_kind,
            alias.value.clone(),
            canonical_path,
            &self.resource_index,
            format!("runtime_mapping:{}={}", alias.kind.label(), alias.value),
            255,
        );
    }

    fn apply_net_runtime_events(
        &mut self,
        timestamp: f64,
        events: &[NetRuntimeEvent],
    ) -> (Vec<String>, bool) {
        let mut notes = Vec::new();
        let mut applied = false;
        for event in events {
            applied |= self.apply_net_runtime_event(timestamp, event);
            notes.push(event.summary());
        }
        (notes, applied)
    }

    fn apply_monster_gameplay_effect_links(
        &mut self,
        timestamp: f64,
        effects: &[(String, usize, u8)],
    ) -> bool {
        let mut applied = false;
        for (effect_name, byte_offset, bit_shift) in effects {
            let lower = effect_name.to_ascii_lowercase();
            if !lower.contains("dmg")
                || lower.contains("steal")
                || lower.contains("player")
                || lower.contains("buff")
            {
                continue;
            }
            let Some(target_path) = self
                .resource_index
                .canonical_target_path_for_gameplay_effect(effect_name)
            else {
                continue;
            };
            if self
                .resource_index
                .resolved_name_for_gameplay_effect(effect_name)
                .is_none()
                && self
                    .resource_index
                    .resolved_name_for_path(&target_path)
                    .is_none()
            {
                continue;
            }
            applied |= self.object_state.link_unique_active_boss_hp_handle_to_path(
                timestamp,
                &target_path,
                &self.resource_index,
                format!(
                    "monster_gameplay_effect:{}@{}:{}",
                    effect_name, byte_offset, bit_shift
                ),
            );
        }
        applied
    }

    fn apply_packet_target_path_links(
        &mut self,
        timestamp: f64,
        path_candidates: &[&PathCandidate],
    ) -> bool {
        let mut candidates = path_candidates
            .iter()
            .filter_map(|candidate| {
                let raw_is_target = !is_ignored_non_target_path(&candidate.value)
                    && crate::object_state::is_targetish_path(&candidate.value)
                    && self
                        .resource_index
                        .resolved_name_for_path(&candidate.value)
                        .is_some();
                let path = if raw_is_target {
                    candidate.value.clone()
                } else {
                    self.resource_index
                        .canonical_target_path_for_path(&candidate.value)
                        .unwrap_or_else(|| candidate.value.clone())
                };
                if is_ignored_non_target_path(&path)
                    || !crate::object_state::is_targetish_path(&path)
                {
                    return None;
                }
                let name = self
                    .resource_index
                    .resolved_name_for_path(&path)
                    .or_else(|| self.resource_index.resolved_name_for_path(&candidate.value))?;
                Some((path, name))
            })
            .collect::<Vec<_>>();
        candidates.sort();
        candidates.dedup();

        let distinct_names = candidates
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<HashSet<_>>();
        if distinct_names.len() != 1 {
            return false;
        }

        let Some((path, _)) = candidates
            .iter()
            .max_by_key(|(path, _)| packet_target_path_rank(path))
        else {
            return false;
        };
        self.object_state.link_unique_active_boss_hp_handle_to_path(
            timestamp,
            path,
            &self.resource_index,
            format!("packet_target_path:{path}"),
        )
    }

    fn apply_net_runtime_event(&mut self, timestamp: f64, event: &NetRuntimeEvent) -> bool {
        let mut applied = false;
        let mut sdk_target_dead = false;
        if event.kind == NetRuntimeEventKind::SdkTargetData
            && let (Some(target_token), Some(current_hp)) =
                (event.target_token.as_deref(), event.current_hp)
        {
            sdk_target_dead =
                current_hp <= 1.0 || event.dead_state.is_some_and(|dead_state| dead_state != 0);
            self.object_state.observe_net_target_hp_update(
                timestamp,
                "sdk_target",
                target_token,
                current_hp as f64,
                None,
                format!("sdk_target_data:{}", event.evidence),
            );
            let instance_id = self.target_instances.observe_sdk_net_target(
                timestamp,
                target_token,
                current_hp as f64,
                format!("sdk_target_data:{}", event.evidence),
            );
            if sdk_target_dead {
                let token = hex::encode(target_token);
                let mut keys = runtime_mapping_alias_keys(&event.aliases);
                keys.push(format!("sdk_net_target:{token}"));
                keys.push(format!("NetRefHandleCandidate:sdk_target:{token}"));
                self.remove_target_handle_alias_keys(keys);
            } else if let Some(instance_id) = instance_id {
                self.register_target_instance_alias_keys(
                    runtime_mapping_alias_keys(&event.aliases),
                    &instance_id,
                    timestamp,
                );
            }
            applied = true;
        }

        if matches!(
            event.action,
            NetRuntimeAction::Close | NetRuntimeAction::Destroy
        ) && let Some((canonical_path, _)) = self.net_runtime_event_target(event)
        {
            for alias_key in self
                .target_instances
                .expire_path(timestamp, &canonical_path)
                .into_iter()
                .flat_map(|key| equivalent_target_alias_keys(&key))
            {
                self.target_handle_aliases.remove(&alias_key);
            }
            applied = true;
        }

        if event.aliases.is_empty() {
            return applied;
        }
        if sdk_target_dead {
            return applied;
        }
        let Some((canonical_path, target_name)) = self.net_runtime_event_target(event) else {
            return applied;
        };
        let instance_id = self.target_instances.observe_runtime_mapping(
            timestamp,
            canonical_path.clone(),
            target_name,
            event.aliases.clone(),
        );
        for alias in &event.aliases {
            self.observe_runtime_alias_in_object_state(timestamp, alias, &canonical_path);
        }
        self.register_target_instance_alias_keys(
            runtime_mapping_alias_keys(&event.aliases),
            &instance_id,
            timestamp,
        );
        true
    }

    fn net_runtime_event_target(&self, event: &NetRuntimeEvent) -> Option<(String, String)> {
        let raw_path = event.path.as_deref()?;
        if is_ignored_non_target_path(raw_path) {
            return None;
        }
        let canonical_path = self
            .resource_index
            .canonical_target_path_for_path(raw_path)
            .unwrap_or_else(|| raw_path.to_owned());
        let target_name = self
            .resource_index
            .resolved_name_for_path(raw_path)
            .or_else(|| self.resource_index.resolved_name_for_path(&canonical_path))
            .or_else(|| self.resource_index.display_name_for_path(&canonical_path))?;
        Some((canonical_path, target_name))
    }
}

fn object_handle_kind_for_runtime_alias(kind: TargetAliasKind) -> Option<ObjectHandleKind> {
    match kind {
        TargetAliasKind::NetGuid32 | TargetAliasKind::NetGuidPacked => {
            Some(ObjectHandleKind::NetGuidCandidate)
        }
        TargetAliasKind::IrisRef32
        | TargetAliasKind::CurrentHpToken
        | TargetAliasKind::SdkNetTarget => Some(ObjectHandleKind::NetRefHandleCandidate),
        TargetAliasKind::BossHpGuid => Some(ObjectHandleKind::AttributeGuid),
        TargetAliasKind::ActorChannel
        | TargetAliasKind::HitTargetToken
        | TargetAliasKind::HitVectorToken => None,
    }
}

fn runtime_mapping_alias_keys(aliases: &[TargetAlias]) -> Vec<String> {
    let mut keys = Vec::new();
    for alias in aliases {
        extend_equivalent_target_alias_keys(&mut keys, &alias.key());
        match alias.kind {
            TargetAliasKind::NetGuid32 | TargetAliasKind::NetGuidPacked => {
                extend_equivalent_target_alias_keys(
                    &mut keys,
                    &format!("NetGuidCandidate:{}", alias.value),
                );
            }
            TargetAliasKind::IrisRef32 => {
                extend_equivalent_target_alias_keys(
                    &mut keys,
                    &format!("NetRefHandleCandidate:{}", alias.value),
                );
            }
            TargetAliasKind::CurrentHpToken => {
                extend_equivalent_target_alias_keys(
                    &mut keys,
                    &format!("NetRefHandleCandidate:currenthp:{}", alias.value),
                );
            }
            TargetAliasKind::SdkNetTarget => {
                extend_equivalent_target_alias_keys(
                    &mut keys,
                    &format!("NetRefHandleCandidate:sdk_target:{}", alias.value),
                );
            }
            TargetAliasKind::BossHpGuid => {
                extend_equivalent_target_alias_keys(
                    &mut keys,
                    &format!("AttributeGuid:{}", alias.value),
                );
            }
            TargetAliasKind::ActorChannel
            | TargetAliasKind::HitTargetToken
            | TargetAliasKind::HitVectorToken => {}
        }
    }
    keys
}

fn target_handle_aliases_from_summary(
    summary: &TargetResolutionSummary,
    timestamp: f64,
) -> Vec<(String, HandleTargetAlias)> {
    let Some(alias) = handle_target_alias_from_summary(summary, timestamp) else {
        return Vec::new();
    };
    target_alias_lookup_keys(summary.target_id.as_ref(), &summary.target_context)
        .into_iter()
        .map(|key| (key, alias.clone()))
        .collect()
}

fn handle_target_alias_from_summary(
    summary: &TargetResolutionSummary,
    timestamp: f64,
) -> Option<HandleTargetAlias> {
    if !has_runtime_target_source(&summary.target_context)
        || has_unreliable_target_name_source(&summary.target_context)
    {
        return None;
    }
    let target_name = summary.target_name.as_ref()?.trim();
    if target_name.is_empty() {
        return None;
    }
    let target_path = target_context_value(&summary.target_context, "target_path")?;
    if is_ignored_non_target_path(target_path) {
        return None;
    }
    Some(HandleTargetAlias {
        target_path: target_path.to_owned(),
        target_name: target_name.to_owned(),
        source: "table_resolved_handle_alias".to_owned(),
        first_seen_at: timestamp.to_bits(),
        last_seen_at: timestamp.to_bits(),
    })
}

fn merge_target_handle_alias(
    aliases: &mut HashMap<String, HandleTargetAlias>,
    target_id: String,
    alias: HandleTargetAlias,
) {
    for target_id in equivalent_target_alias_keys(&target_id) {
        aliases
            .entry(target_id)
            .and_modify(|existing| {
                if !existing
                    .target_path
                    .eq_ignore_ascii_case(&alias.target_path)
                    || existing.target_name != alias.target_name
                {
                    existing.last_seen_at = existing.last_seen_at.max(alias.last_seen_at);
                    return;
                }
                existing.target_path = alias.target_path.clone();
                existing.target_name = alias.target_name.clone();
                existing.source = alias.source.clone();
                existing.first_seen_at = existing.first_seen_at.min(alias.first_seen_at);
                existing.last_seen_at = existing.last_seen_at.max(alias.last_seen_at);
            })
            .or_insert_with(|| alias.clone());
    }
}

fn apply_handle_alias_backfill(
    recent: &mut RecentTargetHit,
    aliases: &HashMap<String, HandleTargetAlias>,
) -> bool {
    if !can_register_target_handle_alias_from_hit(&recent.hit) {
        return false;
    }
    let named_overwrite = recent.hit.target_name.is_some();
    let current_name = recent.hit.target_name.as_deref();
    let current_path = target_context_value(&recent.hit.target_context, "target_path");
    let lookup_keys = target_alias_lookup_keys(
        recent
            .hit
            .target_id
            .as_ref()
            .or(recent.summary.target_id.as_ref()),
        &recent.hit.target_context,
    )
    .into_iter()
    .filter(|key| !is_hp_alias_key(key))
    .collect::<Vec<_>>();
    let Some(alias) = lookup_keys.iter().find_map(|key| aliases.get(key)) else {
        return false;
    };
    if current_name == Some(alias.target_name.as_str())
        && current_path == Some(alias.target_path.as_str())
    {
        return false;
    }
    if named_overwrite && !alias_matches_existing_target_path(current_path, &alias.target_path) {
        return false;
    }
    if recent.hit.target_name.is_some()
        && recent.summary.direct_hp_evidence
        && recent.summary.confidence.rank() >= TargetConfidence::Confirmed.rank()
    {
        return false;
    }
    apply_alias_to_hit(&mut recent.hit, alias);
    recent.summary.target_name = recent.hit.target_name.clone();
    recent.summary.target_context = recent.hit.target_context.clone();
    recent.summary.score = recent.summary.score.max(80);
    if recent.summary.confidence.rank() < TargetConfidence::Probable.rank() {
        recent.summary.confidence = TargetConfidence::Probable;
    }
    true
}

fn alias_matches_existing_target_path(current_path: Option<&str>, alias_path: &str) -> bool {
    current_path.is_some_and(|path| path.eq_ignore_ascii_case(alias_path))
}

fn hits_share_hp_target_handle(left: &Hit, right: &Hit) -> bool {
    let left_keys = target_alias_lookup_keys(left.target_id.as_ref(), &left.target_context)
        .into_iter()
        .filter(|key| is_hp_alias_key(key))
        .collect::<HashSet<_>>();
    !left_keys.is_empty()
        && target_alias_lookup_keys(right.target_id.as_ref(), &right.target_context)
            .into_iter()
            .filter(|key| is_hp_alias_key(key))
            .any(|key| left_keys.contains(&key))
}

fn suppress_hit_target_as_recent_death(hit: &mut Hit, summary: &mut TargetResolutionSummary) {
    hit.target_id = None;
    hit.target_name = None;
    hit.target_context.retain(|entry| {
        !entry.starts_with("target_path=")
            && !entry.starts_with("target_name=")
            && !entry.starts_with("target_name_resolution=")
            && !entry.starts_with("target_name_candidates=")
            && !entry.starts_with("reason=near_object_path:")
            && entry != "reason=resolved_target_name_table"
    });
    push_unique_context(
        &mut hit.target_context,
        "reason=recent_death_suppressed_stale_target".to_owned(),
    );
    summary.target_id = None;
    summary.target_name = None;
    summary.target_context = hit.target_context.clone();
    summary.score = 0;
    summary.confidence = TargetConfidence::Unknown;
    summary.direct_hp_evidence = false;
}

fn normalize_final_target_identity(hit: &mut Hit) {
    if let Some(track_id) = target_context_value(&hit.target_context, "target_track_id") {
        hit.target_id = Some(track_id.to_owned());
        return;
    }
    let Some(target_id) = hit.target_id.clone() else {
        return;
    };
    if is_legacy_handle_candidate_id(&target_id) {
        push_unique_context(
            &mut hit.target_context,
            format!("target_handle_candidate={target_id}"),
        );
        hit.target_id = None;
    }
}

fn net_runtime_events_have_identity(events: &[NetRuntimeEvent]) -> bool {
    events.iter().any(|event| {
        !event.aliases.is_empty()
            || event.path.is_some()
            || event.current_hp.is_some()
            || event.target_token.is_some()
    })
}

fn annotate_unknown_identity_gap(
    hit: &mut Hit,
    no_targetish_path: bool,
    no_net_identity: bool,
    no_runtime_identity: bool,
) {
    if hit.direction == "incoming"
        || hit.target_name.is_some()
        || target_context_value(&hit.target_context, "target_track_id").is_some()
        || target_context_value(&hit.target_context, "target_unresolved").is_some()
        || !no_targetish_path
        || !no_net_identity
        || !no_runtime_identity
    {
        return;
    }
    push_unique_context(
        &mut hit.target_context,
        "target_unresolved=network_identity_not_decoded".to_owned(),
    );
    push_unique_context(
        &mut hit.target_context,
        "target_identity_gap=actor_or_iris_config_id_not_decoded".to_owned(),
    );
}

fn apply_handle_alias_to_hit_summary(
    hit: &mut Hit,
    summary: &mut TargetResolutionSummary,
    aliases: &HashMap<String, HandleTargetAlias>,
) -> bool {
    if !can_register_target_handle_alias_from_hit(hit) {
        return false;
    }
    if hit.target_name.is_some() {
        return false;
    }
    let Some(alias) = target_alias_lookup_keys(
        hit.target_id.as_ref().or(summary.target_id.as_ref()),
        &hit.target_context,
    )
    .into_iter()
    .filter(|key| !is_hp_alias_key(key))
    .find_map(|key| aliases.get(&key)) else {
        return false;
    };
    apply_alias_to_hit(hit, alias);
    summary.target_name = hit.target_name.clone();
    summary.target_context = hit.target_context.clone();
    true
}

fn can_register_target_handle_alias_from_hit(hit: &Hit) -> bool {
    hit.target_hp_after > 1.0 && !has_unreliable_target_name_source(&hit.target_context)
}

fn has_recent_death_suppressed_target(context: &[String]) -> bool {
    context
        .iter()
        .any(|entry| entry == "reason=recent_death_suppressed_stale_target")
}

fn has_unreliable_target_name_source(context: &[String]) -> bool {
    context.iter().any(|entry| {
        matches!(
            entry.as_str(),
            "reason=recent_death_suppressed_stale_target"
                | "reason=hp_handle_path_without_direct_hp_suppressed"
                | "reason=path_only_target_name_suppressed"
                | "reason=net_identity_path_anchor_unconfirmed"
                | "reason=runtime_hp_timeline"
                | "reason=runtime_unique_active_named_instance"
        )
    })
}

fn has_runtime_target_source(context: &[String]) -> bool {
    context
        .iter()
        .any(|entry| entry.starts_with("reason=runtime_alias:"))
}

fn apply_alias_to_hit(hit: &mut Hit, alias: &HandleTargetAlias) {
    hit.target_name = Some(alias.target_name.clone());
    hit.target_context.retain(|entry| {
        !entry.starts_with("target_name_candidates=")
            && !entry.starts_with("reason=near_object_path:")
            && entry != "reason=conflict:multiple_candidates"
    });
    push_unique_context(
        &mut hit.target_context,
        format!("reason=hp_handle_alias_target_path:{}", alias.target_path),
    );
    replace_target_path_context(&mut hit.target_context, &alias.target_path);
    replace_target_context_value(&mut hit.target_context, "target_name", &alias.target_name);
    replace_target_context_value(
        &mut hit.target_context,
        "target_name_resolution",
        "handle_alias_applied",
    );
}

fn replace_target_context_value(context: &mut Vec<String>, key: &str, value: &str) {
    let prefix = format!("{key}=");
    context.retain(|entry| !entry.starts_with(&prefix));
    push_unique_context(context, format!("{key}={value}"));
}

fn replace_target_path_context(context: &mut Vec<String>, target_path: &str) {
    let mut ignored_paths = Vec::new();
    context.retain(|entry| {
        let Some(path) = entry.strip_prefix("target_path=") else {
            return true;
        };
        if is_ignored_non_target_path(path) {
            ignored_paths.push(path.to_owned());
        }
        false
    });
    for path in ignored_paths {
        push_unique_context(context, format!("ignored_non_target_path={path}"));
    }
    push_unique_context(context, format!("target_path={target_path}"));
}

fn push_unique_context(context: &mut Vec<String>, value: String) {
    if !context.iter().any(|entry| entry == &value) {
        context.push(value);
    }
}

fn extend_equivalent_target_alias_keys(keys: &mut Vec<String>, key: &str) {
    for key in equivalent_target_alias_keys(key) {
        if !keys.iter().any(|existing| existing == &key) {
            keys.push(key);
        }
    }
}

fn should_apply_target_update(
    old: &TargetResolutionSummary,
    new: &TargetResolutionSummary,
) -> bool {
    if has_recent_death_suppressed_target(&old.target_context) && new.target_name.is_some() {
        return false;
    }
    if let (Some(old_name), Some(new_name)) =
        (old.target_name.as_deref(), new.target_name.as_deref())
        && old_name != new_name
    {
        return false;
    }
    if old.target_id.is_some()
        && new.target_id.is_some()
        && old.target_id != new.target_id
        && !new.direct_hp_evidence
    {
        return false;
    }
    if old.target_id == new.target_id
        && new.target_id.is_some()
        && old.target_name.is_none()
        && new.target_name.is_some()
    {
        return true;
    }
    if named_target_paths_conflict(old, new) {
        return false;
    }
    if old.target_id == new.target_id
        && new.target_id.is_some()
        && old.target_name != new.target_name
        && new.target_name.is_some()
        && new.confidence.rank() >= old.confidence.rank()
    {
        return true;
    }
    if old.target_name.is_none() && new.target_name.is_some() {
        return true;
    }
    if new.confidence.rank() < old.confidence.rank() {
        return false;
    }
    if old.target_id.is_some()
        && new.target_id.is_some()
        && old.target_id != new.target_id
        && old.confidence.rank() >= TargetConfidence::Probable.rank()
    {
        return new.direct_hp_evidence && new.confidence.rank() >= old.confidence.rank();
    }
    if new.confidence.rank() > old.confidence.rank() {
        return true;
    }
    if old.target_id.is_none() && new.target_id.is_some() {
        return true;
    }
    if old.target_id == new.target_id && new.target_id.is_some() && new.score > old.score {
        return true;
    }
    old.target_id != new.target_id
        && new.target_id.is_some()
        && new.direct_hp_evidence
        && new.confidence.rank() >= old.confidence.rank()
}

fn named_target_paths_conflict(
    old: &TargetResolutionSummary,
    new: &TargetResolutionSummary,
) -> bool {
    if old.target_name.is_none() || new.target_name.is_none() || old.target_name == new.target_name
    {
        return false;
    }
    let old_path = target_context_value(&old.target_context, "target_path");
    let new_path = target_context_value(&new.target_context, "target_path");
    match (old_path, new_path) {
        (Some(old_path), Some(new_path)) => !old_path.eq_ignore_ascii_case(new_path),
        (Some(_), None) | (None, Some(_)) => true,
        (None, None) => false,
    }
}

fn should_preserve_existing_target_id_for_name_update(
    old: &TargetResolutionSummary,
    new: &TargetResolutionSummary,
) -> bool {
    old.target_name.is_none()
        && new.target_name.is_some()
        && old.target_id.is_some()
        && new.target_id.is_some()
        && old.target_id != new.target_id
        && old.confidence.rank() >= TargetConfidence::Probable.rank()
        && !(new.direct_hp_evidence && new.confidence.rank() >= old.confidence.rank())
}

fn matching_gameplay_effect<'a>(
    hit: &Hit,
    effects: &'a [ParsedGameplayEffect],
) -> Option<&'a ParsedGameplayEffect> {
    let mut aligned = effects
        .iter()
        .filter(|effect| effect.bit_shift == hit.bit_shift);
    let first = aligned.next();
    if first.is_some() && aligned.next().is_none() {
        first
    } else if effects.len() == 1 {
        effects.first()
    } else {
        None
    }
}

fn enrich_hit_with_gameplay_effect(
    hit: &mut Hit,
    effects: &[ParsedGameplayEffect],
    names: &HashMap<u32, String>,
    skills: &HashMap<String, GameplayEffectSkill>,
    wooden_names: &HashMap<String, String>,
) {
    let Some(effect) = matching_gameplay_effect(hit, effects) else {
        return;
    };
    hit.gameplay_effect_index = Some(effect.unique_index);
    let Some(effect_name) = names.get(&effect.unique_index) else {
        return;
    };
    hit.gameplay_effect_name = Some(effect_name.clone());
    hit.damage_name = resolve_damage_name(effect_name, skills, wooden_names);
    if let Some(skill) = skills.get(effect_name) {
        hit.ability_name = skill.ability_name.clone();
        hit.attack_type = Some(skill.attack_type.clone());
    } else if let Some(attack_type) = hit
        .damage_name
        .as_deref()
        .and_then(classify_attack_type_from_description)
    {
        hit.attack_type = Some(attack_type);
    } else {
        hit.attack_type = Some(classify_attack_type(None, effect_name, None));
    }
}

fn resolve_damage_name(
    effect_name: &str,
    skills: &HashMap<String, GameplayEffectSkill>,
    wooden_names: &HashMap<String, String>,
) -> Option<String> {
    if let Some(name) = wooden_names.get(effect_name) {
        return Some(name.clone());
    }
    let ability_name = skills.get(effect_name)?.ability_name.as_deref()?;
    let mut names = skills
        .iter()
        .filter(|(_, skill)| skill.ability_name.as_deref() == Some(ability_name))
        .filter_map(|(candidate_effect, _)| wooden_names.get(candidate_effect))
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    (names.len() == 1).then(|| names.remove(0))
}

impl PacketDecoder {
    fn process_ethernet_frame(
        &mut self,
        packet: &[u8],
        timestamp: f64,
        local_ip: Option<Ipv4Addr>,
        include_incoming: bool,
        characters: &HashMap<u32, CharacterInfo>,
        sender: &Sender<EngineEvent>,
    ) {
        let Some((src, src_port, dst, dst_port, payload)) = parse_udp_ipv4(packet) else {
            return;
        };
        if local_ip.is_some_and(|ip| src != ip && dst != ip) {
            return;
        }

        let evidence = find_declared_character_evidence(payload);
        let ids = declared_character_ids_from_evidence(&evidence);
        self.follow_up_damage
            .observe_characters(ids.iter().copied(), characters);
        let outgoing = infer_outgoing(src, src_port, dst, local_ip, &ids, &self.client_endpoints);
        if outgoing && !ids.is_empty() {
            self.client_endpoints.insert((src, src_port));
        }
        let direction = if outgoing { "C2S" } else { "S2C" };
        let current_hp_updates = if outgoing {
            Vec::new()
        } else {
            parse_current_hp_updates(payload)
        };
        let boss_hp_updates = if outgoing {
            Vec::new()
        } else {
            parse_boss_hp_updates(payload)
        };
        let payload_scan = PayloadScan::new(payload);
        let has_fast_text_hint = payload_scan.contains_any_fast_hint();
        if should_fast_skip_payload(
            payload,
            &ids,
            !current_hp_updates.is_empty() || !boss_hp_updates.is_empty(),
            has_fast_text_hint,
        ) {
            return;
        }

        let decoded_text = if has_fast_text_hint {
            payload_scan.decode_text()
        } else {
            UNREADABLE_PROTOCOL_TEXT.to_owned()
        };
        let path_candidates = if !outgoing && has_fast_text_hint {
            extract_path_candidates(payload)
        } else {
            Vec::new()
        };
        for candidate in &path_candidates {
            self.object_state
                .observe_path_candidate(timestamp, candidate, &self.resource_index);
        }
        let net_identity_candidates = extract_net_identity_candidates(payload, &path_candidates);
        let target_instance_notes = self.target_instances.observe_paths(
            timestamp,
            &path_candidates,
            &net_identity_candidates,
            &self.resource_index,
        );
        for candidate in &net_identity_candidates {
            let handle_kind = match candidate.kind {
                NetIdentityCandidateKind::NetGuidPacked | NetIdentityCandidateKind::NetGuid32 => {
                    ObjectHandleKind::NetGuidCandidate
                }
                NetIdentityCandidateKind::IrisNetRefHandle32 => {
                    ObjectHandleKind::NetRefHandleCandidate
                }
            };
            self.object_state.observe_path_handle_candidate(
                timestamp,
                handle_kind,
                candidate.handle.clone(),
                &candidate.path,
                &self.resource_index,
                format!(
                    "net_identity:{}={} path_anchor:{}@{}:{} rel={} raw={}",
                    candidate.kind.label(),
                    candidate.handle,
                    candidate.path,
                    candidate.byte_offset,
                    candidate.bit_shift,
                    candidate.relative_offset,
                    candidate.raw_hex
                ),
                candidate.score,
            );
        }
        let gameplay_effects = if has_fast_text_hint {
            parse_gameplay_effects(payload)
        } else {
            Vec::new()
        };
        let targetish_path_candidates = path_candidates
            .iter()
            .filter(|candidate| {
                self.resource_index
                    .resolved_name_for_path(&candidate.value)
                    .is_some()
                    || crate::object_state::is_targetish_path(&candidate.value)
            })
            .collect::<Vec<_>>();
        let packet_target_path_link_applied =
            self.apply_packet_target_path_links(timestamp, &targetish_path_candidates);
        let transport_packet = parse_transport_packet(payload);
        let single_bunch = match &transport_packet {
            Some(TransportPacket::Sequenced(packet)) => parse_single_bunch(packet),
            _ => None,
        };
        let sdk_target_scan_enabled = !outgoing
            && (!current_hp_updates.is_empty()
                || !boss_hp_updates.is_empty()
                || !targetish_path_candidates.is_empty()
                || !gameplay_effects.is_empty()
                || has_net_runtime_text_hint(&decoded_text));
        let net_runtime_events = extract_net_runtime_events(
            payload,
            &path_candidates,
            &net_identity_candidates,
            transport_packet.as_ref(),
            single_bunch.as_ref(),
            NetRuntimeScanOptions {
                include_text_markers: decoded_text != UNREADABLE_PROTOCOL_TEXT
                    || !targetish_path_candidates.is_empty(),
                include_sdk_target_data: sdk_target_scan_enabled,
            },
        );
        let (net_runtime_notes, net_runtime_applied) =
            self.apply_net_runtime_events(timestamp, &net_runtime_events);
        self.apply_hp_updates_to_state(timestamp, &current_hp_updates, &boss_hp_updates);
        let (packet_char_id, fallback_char_id) = if outgoing {
            let session_key = (src, src_port, dst, dst_port);
            let packet_char_id = if ids.len() == 1 {
                ids.first().copied()
            } else {
                None
            };
            if let Some(id) = packet_char_id {
                self.session_characters.insert(session_key, id);
            }
            let fallback = self.session_characters.get(&session_key).copied();
            (packet_char_id, fallback)
        } else {
            (None, None)
        };
        let mut hits = if outgoing
            && payload.len() >= DAMAGE_RECORD_MIN_LEN
            && payload_scan.may_contain_damage_record()
        {
            parse_damage_payload(
                payload,
                timestamp,
                packet_char_id,
                fallback_char_id,
                characters,
                &evidence,
            )
        } else {
            Vec::new()
        };
        for hit in &mut hits {
            attach_runtime_alias_context(hit, &net_identity_candidates, &net_runtime_events);
            enrich_hit_with_gameplay_effect(
                hit,
                &gameplay_effects,
                &self.gameplay_effect_names,
                &self.gameplay_effect_skills,
                &self.wooden_damage_names,
            );
            if hit
                .attack_type
                .as_deref()
                .is_some_and(|attack_type| attack_type.starts_with("环合"))
            {
                let previous_declared_character = self
                    .character_declarations
                    .iter()
                    .filter(|(character_id, declared_at)| {
                        **character_id != hit.char_id && timestamp - **declared_at <= 3.0
                    })
                    .max_by(|left, right| left.1.total_cmp(right.1))
                    .map(|(character_id, _)| *character_id);
                let previous_attribute = previous_declared_character
                    .and_then(|character_id| characters.get(&character_id))
                    .and_then(|character| character.attribute.as_deref());
                let entering_attribute = characters
                    .get(&hit.char_id)
                    .and_then(|character| character.attribute.as_deref());
                if let (Some(previous_attribute), Some(entering_attribute)) =
                    (previous_attribute, entering_attribute)
                    && let Some(reaction_type) = qte_reaction_type(
                        previous_attribute,
                        entering_attribute,
                        &self.follow_up_damage.team_attributes,
                    )
                {
                    hit.attack_type = Some(format!("环合·{reaction_type}"));
                }
            }
        }
        hits.retain(|hit| !is_non_player_damage_effect(hit.gameplay_effect_name.as_deref()));

        let target_lock_context = target_lock_context_for_packet(
            &path_candidates,
            &current_hp_updates,
            &boss_hp_updates,
            &self.resource_index,
        );
        let mut hit_summaries = Vec::with_capacity(hits.len());
        for hit in &mut hits {
            let mut summary = if hit.direction != "incoming" {
                self.target_resolver.apply_to_hit_with_summary(
                    hit,
                    &self.object_state,
                    &self.target_instances,
                    &path_candidates,
                    &self.resource_index,
                )
            } else {
                TargetResolutionSummary::from_hit_and_candidates(hit, &[])
            };
            if hit.direction != "incoming" {
                self.suppress_recently_dead_target_on_hit(hit, &mut summary);
                self.register_target_handle_alias_for_hit(hit, &summary);
                apply_handle_alias_to_hit_summary(hit, &mut summary, &self.target_handle_aliases);
                self.register_target_handle_alias_for_hit(hit, &summary);
            }
            hit_summaries.push(summary);
        }
        for (hit, summary) in hits.iter_mut().zip(&mut hit_summaries) {
            if hit.direction != "incoming" {
                apply_handle_alias_to_hit_summary(hit, summary, &self.target_handle_aliases);
            }
        }
        let mut packet_hit_order = (0..hits.len()).collect::<Vec<_>>();
        packet_hit_order.sort_by(|left, right| {
            hits[*left]
                .timestamp
                .total_cmp(&hits[*right].timestamp)
                .then_with(|| hits[*left].byte_offset.cmp(&hits[*right].byte_offset))
                .then_with(|| hits[*left].bit_shift.cmp(&hits[*right].bit_shift))
        });
        let mut packet_dead_hits: Vec<Hit> = Vec::new();
        for &index in &packet_hit_order {
            let hit = &mut hits[index];
            let summary = &mut hit_summaries[index];
            if hit.direction != "incoming"
                && hit.target_hp_after > 1.0
                && packet_dead_hits.iter().any(|dead_hit| {
                    hit.timestamp >= dead_hit.timestamp
                        && hit.timestamp - dead_hit.timestamp
                            <= DEAD_TARGET_HANDLE_REUSE_WINDOW_SECONDS
                        && hits_share_hp_target_handle(hit, dead_hit)
                })
            {
                suppress_hit_target_as_recent_death(hit, summary);
            }
            if hit.direction != "incoming" && hit.target_hp_after <= 1.0 {
                packet_dead_hits.push(hit.clone());
            }
        }
        for &index in &packet_hit_order {
            let hit = &mut hits[index];
            let summary = &mut hit_summaries[index];
            self.apply_target_track_attribution(hit, summary, target_lock_context.clone());
            annotate_unknown_identity_gap(
                hit,
                targetish_path_candidates.is_empty(),
                net_identity_candidates.is_empty(),
                !net_runtime_events_have_identity(&net_runtime_events),
            );
        }
        for index in packet_hit_order {
            let hit = &mut hits[index];
            let summary = &mut hit_summaries[index];
            if hit.direction != "incoming" && hit.target_hp_after <= 1.0 {
                self.clear_dead_target_handles_from_hit(hit);
                summary.target_context = hit.target_context.clone();
            }
        }
        let monster_gameplay_effect_links = hits
            .iter()
            .filter(|hit| hit.direction == "incoming")
            .filter_map(|hit| {
                hit.gameplay_effect_name
                    .clone()
                    .map(|name| (name, hit.byte_offset, hit.bit_shift))
            })
            .collect::<Vec<_>>();
        for character_id in &ids {
            self.character_declarations.insert(*character_id, timestamp);
        }
        self.character_declarations
            .retain(|_, declared_at| timestamp - *declared_at <= 10.0);
        let accepted = hits
            .iter()
            .filter(|hit| include_incoming || hit.direction != "incoming")
            .count();
        let monster_gameplay_effect_applied =
            self.apply_monster_gameplay_effect_links(timestamp, &monster_gameplay_effect_links);
        if !current_hp_updates.is_empty()
            || !boss_hp_updates.is_empty()
            || !targetish_path_candidates.is_empty()
            || packet_target_path_link_applied
            || net_runtime_applied
            || monster_gameplay_effect_applied
        {
            self.backfill_recent_targets(timestamp, sender);
        }
        let abyss_events = abyss_events_from_text(timestamp, &decoded_text);
        let has_targetish_or_runtime = !targetish_path_candidates.is_empty()
            || !net_identity_candidates.is_empty()
            || !net_runtime_events.is_empty()
            || packet_target_path_link_applied
            || net_runtime_applied
            || monster_gameplay_effect_applied;
        let should_emit_packet_debug = self.packet_debug_mode != PacketDebugMode::Off
            && should_keep_debug_packet(
                accepted,
                !current_hp_updates.is_empty() || !boss_hp_updates.is_empty(),
                has_targetish_or_runtime,
                !abyss_events.is_empty(),
                self.packet_debug_mode,
            );
        if current_hp_updates.is_empty()
            && boss_hp_updates.is_empty()
            && targetish_path_candidates.is_empty()
            && net_runtime_notes.is_empty()
            && accepted == 0
            && abyss_events.is_empty()
            && !should_emit_packet_debug
        {
            return;
        }
        let mut note = if hits.len() != accepted {
            format!("过滤 {} 条 incoming 记录", hits.len() - accepted)
        } else {
            String::new()
        };
        append_packet_note(
            &mut note,
            binary_payload_diagnostic(payload, direction, &decoded_text, &evidence),
        );
        if !targetish_path_candidates.is_empty() {
            append_packet_note(
                &mut note,
                Some(format!(
                    "Object/path candidates: {}",
                    targetish_path_candidates
                        .iter()
                        .take(5)
                        .map(|candidate| format!(
                            "{}@{}:{}",
                            candidate.value, candidate.byte_offset, candidate.bit_shift
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
        if !net_identity_candidates.is_empty() {
            append_packet_note(
                &mut note,
                Some(format!(
                    "Net identity candidates: {}",
                    net_identity_candidates
                        .iter()
                        .take(6)
                        .map(|candidate| format!(
                            "{}:{}->{}@{}:{}",
                            candidate.kind.label(),
                            candidate.handle,
                            candidate.path,
                            candidate.byte_offset,
                            candidate.bit_shift
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
        if !target_instance_notes.is_empty() {
            append_packet_note(
                &mut note,
                Some(format!(
                    "Runtime target instances: {}",
                    target_instance_notes
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
        if !net_runtime_notes.is_empty() {
            append_packet_note(
                &mut note,
                Some(format!(
                    "Net runtime events: {}",
                    net_runtime_notes
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
        let target_notes = hits
            .iter()
            .filter(|hit| !hit.target_context.is_empty())
            .take(3)
            .map(|hit| {
                format!(
                    "target hit {:.0}: {}",
                    hit.damage,
                    hit.target_context
                        .iter()
                        .take(4)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" | ")
                )
            })
            .collect::<Vec<_>>();
        if !target_notes.is_empty() {
            append_packet_note(
                &mut note,
                Some(format!("Target candidates: {}", target_notes.join("; "))),
            );
        }
        if !gameplay_effects.is_empty() {
            append_packet_note(
                &mut note,
                Some(format!(
                    "GameplayEffect: {}",
                    gameplay_effects
                        .iter()
                        .map(|effect| {
                            let location = format!("@{}:{}", effect.byte_offset, effect.bit_shift);
                            self.gameplay_effect_names
                                .get(&effect.unique_index)
                                .map_or_else(
                                    || format!("{}{}", effect.unique_index, location),
                                    |name| format!("{} {}{}", effect.unique_index, name, location),
                                )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
        if !current_hp_updates.is_empty() {
            append_packet_note(
                &mut note,
                Some(format!(
                    "CurrentHP 更新候选：{}",
                    current_hp_updates
                        .iter()
                        .map(|update| format!(
                            "{}={:.0}@{}:{}",
                            current_hp_target_token(&update.target_hint)
                                .map_or_else(|| "untracked".to_owned(), |token| short_hex(&token),),
                            update.current_hp,
                            update.byte_offset,
                            update.bit_shift
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
        if !boss_hp_updates.is_empty() {
            append_packet_note(
                &mut note,
                Some(format!(
                    "Boss HP updates: {}",
                    boss_hp_updates
                        .iter()
                        .map(|update| format!(
                            "{}={:.0}@{}:{}",
                            hex::encode(update.target_handle),
                            update.current_hp,
                            update.byte_offset,
                            update.bit_shift
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
        let inferred_follow_up_hit_values = boss_hp_updates
            .iter()
            .filter_map(|update| {
                self.follow_up_damage
                    .observe_server_hp(timestamp, update.current_hp as f64)
            })
            .collect::<Vec<_>>();
        let mut inferred_follow_up_hits = Vec::with_capacity(inferred_follow_up_hit_values.len());
        for mut hit in inferred_follow_up_hit_values {
            let mut summary = self.target_resolver.apply_to_hit_with_summary(
                &mut hit,
                &self.object_state,
                &self.target_instances,
                &[],
                &self.resource_index,
            );
            self.suppress_recently_dead_target_on_hit(&mut hit, &mut summary);
            inferred_follow_up_hits.push((hit, summary));
        }
        for (hit, summary) in &mut inferred_follow_up_hits {
            self.register_target_handle_alias_for_hit(hit, summary);
            apply_handle_alias_to_hit_summary(hit, summary, &self.target_handle_aliases);
            self.register_target_handle_alias_for_hit(hit, summary);
        }
        for (hit, summary) in &mut inferred_follow_up_hits {
            self.apply_target_track_attribution(hit, summary, target_lock_context.clone());
            annotate_unknown_identity_gap(
                hit,
                targetish_path_candidates.is_empty(),
                true,
                !net_runtime_events_have_identity(&net_runtime_events),
            );
        }
        for (hit, summary) in &mut inferred_follow_up_hits {
            self.clear_dead_target_handles_from_hit(hit);
            summary.target_context = hit.target_context.clone();
        }
        if let Some(TransportPacket::Sequenced(packet)) = &transport_packet {
            if packet.mode != 0 {
                append_packet_note(
                    &mut note,
                    Some(format!(
                        "传输模式 {}，PacketId {}，Ack {}，应用载荷 {} bit",
                        packet.mode,
                        packet.packet_id,
                        packet.acknowledged_packet_id,
                        packet.payload_bit_len
                    )),
                );
            } else if let Some(bunch) = &single_bunch {
                append_packet_note(
                    &mut note,
                    Some(format!(
                        "SingleBunch seq {}，descriptor 0x{:02x}，数据 {} bit",
                        bunch.sequence, bunch.descriptor, bunch.data_bit_len
                    )),
                );
            }
        }
        if should_emit_packet_debug {
            let preview_len = payload.len().min(96);
            let payload_preview = hex::encode(&payload[..preview_len]);
            let payload_hex = if self.packet_debug_mode == PacketDebugMode::FullPayload {
                hex::encode(payload)
            } else {
                String::new()
            };
            send_packet_events(
                sender,
                PacketDebug {
                    timestamp,
                    source: format!("{src}:{src_port}"),
                    destination: format!("{dst}:{dst_port}"),
                    direction: direction.to_owned(),
                    payload_len: payload.len(),
                    declared_ids: ids,
                    parsed_hits: accepted,
                    note,
                    payload_preview,
                    payload_hex,
                    decoded_text,
                },
            );
        } else {
            for event in abyss_events {
                let _ = sender.send(EngineEvent::Abyss(event));
            }
        }
        for (hit, summary) in hits.into_iter().zip(hit_summaries) {
            if include_incoming || hit.direction != "incoming" {
                self.follow_up_damage
                    .observe_hit(&hit, hit.gameplay_effect_index, characters);
                let mut hit = hit;
                normalize_final_target_identity(&mut hit);
                self.remember_target_hit(&hit, summary);
                let _ = sender.send(EngineEvent::Hit(hit));
            }
        }
        for (hit, summary) in inferred_follow_up_hits {
            let mut hit = hit;
            normalize_final_target_identity(&mut hit);
            self.remember_target_hit(&hit, summary);
            let _ = sender.send(EngineEvent::Hit(hit));
        }
    }
}

pub fn start_capture(
    device: CaptureDevice,
    local_ip: Option<Ipv4Addr>,
    filter: String,
    options: CaptureOptions,
    characters: Arc<HashMap<u32, CharacterInfo>>,
    sender: Sender<EngineEvent>,
) -> CaptureHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let raw_capture = options
        .save_raw_capture
        .then(|| RawCaptureBuffer::new(device.clone()));
    let thread_raw_capture = raw_capture
        .clone()
        .unwrap_or_else(RawCaptureBuffer::disabled);
    let thread = thread::spawn(move || {
        let result = run_capture(CaptureRunConfig {
            device: &device,
            local_ip,
            filter: &filter,
            include_incoming: options.include_incoming,
            characters: &characters,
            sender: &sender,
            stop: &thread_stop,
            raw_capture: &thread_raw_capture,
            packet_debug_mode: options.packet_debug_mode,
        });
        thread_raw_capture.finish();
        let _ = sender.send(EngineEvent::CaptureStopped);
        if let Err(error) = result {
            let _ = sender.send(EngineEvent::Error(error));
        }
    });
    CaptureHandle {
        stop,
        thread: Some(thread),
        raw_capture,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureOptions {
    pub include_incoming: bool,
    pub packet_debug_mode: PacketDebugMode,
    pub save_raw_capture: bool,
}

struct CaptureRunConfig<'a> {
    device: &'a CaptureDevice,
    local_ip: Option<Ipv4Addr>,
    filter: &'a str,
    include_incoming: bool,
    characters: &'a HashMap<u32, CharacterInfo>,
    sender: &'a Sender<EngineEvent>,
    stop: &'a AtomicBool,
    raw_capture: &'a RawCaptureBuffer,
    packet_debug_mode: PacketDebugMode,
}

fn run_capture(config: CaptureRunConfig<'_>) -> Result<(), String> {
    let CaptureRunConfig {
        device,
        local_ip,
        filter,
        include_incoming,
        characters,
        sender,
        stop,
        raw_capture,
        packet_debug_mode,
    } = config;
    // SAFETY: Function pointers are loaded from Npcap and used per the libpcap API.
    unsafe {
        let _packet_library =
            Library::new(packet_library_path()).map_err(|error| error.to_string())?;
        let library = Library::new(npcap_library_path()).map_err(|error| error.to_string())?;
        let open_live: OpenLive = load_symbol(&library, b"pcap_open_live\0")?;
        let next_ex: NextEx = load_symbol(&library, b"pcap_next_ex\0")?;
        let close: Close = load_symbol(&library, b"pcap_close\0")?;
        let compile: Compile = load_symbol(&library, b"pcap_compile\0")?;
        let set_filter: SetFilter = load_symbol(&library, b"pcap_setfilter\0")?;
        let free_code: FreeCode = load_symbol(&library, b"pcap_freecode\0")?;
        let get_err: GetErr = load_symbol(&library, b"pcap_geterr\0")?;

        let device_name = CString::new(device.name.as_str()).map_err(|error| error.to_string())?;
        let mut error_buffer = [0_i8; PCAP_ERRBUF_SIZE];
        let handle = open_live(
            device_name.as_ptr(),
            65_535,
            1,
            100,
            error_buffer.as_mut_ptr(),
        );
        if handle.is_null() {
            return Err(format!(
                "failed to open device: {}",
                c_string(error_buffer.as_ptr())
            ));
        }
        let handle = PcapHandle::new(handle, close);

        let capture_filter = CString::new(filter).map_err(|error| error.to_string())?;
        let mut program = BpfProgramGuard::new(free_code);
        if compile(
            handle.as_ptr(),
            program.as_mut(),
            capture_filter.as_ptr(),
            1,
            u32::MAX,
        ) != 0
            || set_filter(handle.as_ptr(), program.as_mut()) != 0
        {
            let error = c_string(get_err(handle.as_ptr()));
            return Err(format!("failed to set capture filter: {error}"));
        }
        program.release();
        let raw_capture_status = raw_capture.path().map_or_else(
            || "; raw capture disabled".to_owned(),
            |path| format!("; writing raw capture to {}", path.display()),
        );
        let _ = sender.send(EngineEvent::Status(format!(
            "capturing: {} ({}){}",
            device.description,
            local_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "local IP not filtered".to_owned()),
            raw_capture_status
        )));

        let mut decoder = PacketDecoder::with_packet_debug_mode(packet_debug_mode);
        if let Some(warning) = decoder.resource_warning() {
            let _ = sender.send(EngineEvent::Warning(warning));
        }
        while !stop.load(Ordering::Relaxed) {
            let mut header = ptr::null();
            let mut packet_data = ptr::null();
            let result = next_ex(handle.as_ptr(), &mut header, &mut packet_data);
            if result == 0 {
                continue;
            }
            if result < 0 {
                let error = c_string(get_err(handle.as_ptr()));
                return Err(format!("failed to read packet: {error}"));
            }
            if header.is_null() || packet_data.is_null() {
                continue;
            }
            let header_ref = &*header;
            if header_ref.caplen == 0 {
                continue;
            }
            let packet = std::slice::from_raw_parts(packet_data, header_ref.caplen as usize);
            let timestamp =
                header_ref.ts.tv_sec as f64 + header_ref.ts.tv_usec as f64 / 1_000_000.0;
            let raw_timestamp = Duration::new(
                header_ref.ts.tv_sec.max(0) as u64,
                header_ref.ts.tv_usec.clamp(0, 999_999) as u32 * 1_000,
            );
            raw_capture.push(raw_timestamp, header_ref.len, packet);
            decoder.process_ethernet_frame(
                packet,
                timestamp,
                local_ip,
                include_incoming,
                characters,
                sender,
            );
        }
    }
    Ok(())
}

pub fn import_pcapng(
    path: PathBuf,
    characters: Arc<HashMap<u32, CharacterInfo>>,
    local_ip_hint: Option<Ipv4Addr>,
    include_incoming: bool,
    packet_debug_mode: PacketDebugMode,
    sender: Sender<EngineEvent>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let direction_mode = local_ip_hint.map_or_else(
            || "heuristic direction".to_owned(),
            |ip| format!("local IP {ip}"),
        );
        let _ = sender.send(EngineEvent::Status(format!(
            "importing pcapng: {direction_mode}"
        )));
        let result = (|| -> Result<(usize, usize), String> {
            let file = File::open(&path).map_err(|error| error.to_string())?;
            let mut reader = PcapNgReader::new(file).map_err(|error| error.to_string())?;
            let mut decoder = PacketDecoder::with_packet_debug_mode(packet_debug_mode);
            let mut runtime_mapping = load_companion_runtime_mapping(&path, &sender);
            if let Some(warning) = decoder.resource_warning() {
                let _ = sender.send(EngineEvent::Warning(warning));
            }
            let mut packet_count = 0;
            let mut supported_count = 0;

            loop {
                let interface_linktypes = reader
                    .interfaces()
                    .iter()
                    .map(|interface| interface.linktype)
                    .collect::<Vec<_>>();
                let Some(block) = reader.next_block() else {
                    break;
                };
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let block = block.map_err(|error| error.to_string())?;
                let (interface_id, timestamp) = match &block {
                    Block::EnhancedPacket(packet) => {
                        (packet.interface_id as usize, packet.timestamp.as_secs_f64())
                    }
                    Block::SimplePacket(_) => (0, 0.0),
                    _ => continue,
                };
                packet_count += 1;
                let Some(linktype) = interface_linktypes.get(interface_id) else {
                    continue;
                };
                if *linktype != DataLink::ETHERNET {
                    continue;
                }
                supported_count += 1;
                let data = match &block {
                    Block::EnhancedPacket(packet) => packet.data.as_ref(),
                    Block::SimplePacket(packet) => packet.data.as_ref(),
                    _ => continue,
                };
                runtime_mapping.align_to_packet_time(timestamp);
                decoder.apply_runtime_mapping_events(runtime_mapping.pop_due(timestamp), &sender);
                decoder.process_ethernet_frame(
                    data,
                    timestamp,
                    local_ip_hint,
                    include_incoming,
                    &characters,
                    &sender,
                );
            }
            decoder.apply_runtime_mapping_events(runtime_mapping.pop_all(), &sender);
            if packet_count > 0 && supported_count == 0 {
                return Err("pcapng contains no supported Ethernet packets".to_owned());
            }
            Ok((packet_count, supported_count))
        })();

        let _ = sender.send(EngineEvent::CaptureStopped);
        match result {
            Ok((packet_count, supported_count)) => {
                let _ = sender.send(EngineEvent::Status(format!(
                    "pcapng import complete: read {packet_count} packets, parsed {supported_count} Ethernet packets; {direction_mode}"
                )));
            }
            Err(error) => {
                let _ = sender.send(EngineEvent::Error(format!("pcapng import failed: {error}")));
            }
        }
    })
}

fn load_companion_runtime_mapping(
    path: &Path,
    sender: &Sender<EngineEvent>,
) -> RuntimeMappingTimeline {
    let Some(sidecar_path) = find_companion_runtime_mapping_sidecar(path) else {
        return RuntimeMappingTimeline::default();
    };
    match load_runtime_mapping_sidecar(&sidecar_path) {
        Ok(timeline) => {
            let count = timeline.len();
            let _ = sender.send(EngineEvent::Status(format!(
                "loaded runtime mapping sidecar: {} ({count} events)",
                sidecar_path.display()
            )));
            timeline
        }
        Err(error) => {
            let _ = sender.send(EngineEvent::Warning(format!(
                "runtime mapping sidecar ignored: {}: {error}",
                sidecar_path.display()
            )));
            RuntimeMappingTimeline::default()
        }
    }
}

#[derive(Deserialize)]
struct CaptureExport {
    #[serde(default)]
    hits: Vec<ExportHit>,
    #[serde(default)]
    packets: Vec<ExportPacket>,
}

#[derive(Deserialize)]
struct ExportHit {
    timestamp_unix: f64,
    char_id: u32,
    char_name: String,
    damage: f64,
    #[serde(default = "default_outgoing_direction")]
    direction: String,
    #[serde(default)]
    target_hp_before: f64,
    #[serde(default)]
    target_hp_after: f64,
    #[serde(default)]
    target_max_hp: f64,
    #[serde(default)]
    target_hp_percent: f64,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    target_name: Option<String>,
    #[serde(default)]
    target_context: Vec<String>,
    #[serde(default)]
    gameplay_effect_index: Option<u32>,
    #[serde(default)]
    gameplay_effect_name: Option<String>,
    #[serde(default)]
    ability_name: Option<String>,
    #[serde(default)]
    damage_name: Option<String>,
    #[serde(default)]
    attack_type: Option<String>,
}

fn default_outgoing_direction() -> String {
    "outgoing".to_owned()
}

#[derive(Deserialize)]
struct ExportPacket {
    timestamp_unix: f64,
    source: String,
    destination: String,
    #[serde(default)]
    direction: String,
    #[serde(default)]
    payload_len: usize,
    #[serde(default)]
    declared_ids: serde_json::Value,
    #[serde(default)]
    parsed_hits: usize,
    #[serde(default)]
    note: String,
    #[serde(default)]
    payload_preview: String,
    #[serde(default)]
    payload_hex: String,
    #[serde(default)]
    decoded_text: String,
}

pub fn import_capture_json(
    path: PathBuf,
    packet_debug_mode: PacketDebugMode,
    sender: Sender<EngineEvent>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let result = (|| -> Result<(usize, usize), String> {
            let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
            let mut document = parse_capture_export(&text)?;
            drop(text);
            let hit_count = document.hits.len();
            let mut packet_count = 0;
            document
                .packets
                .sort_by(|left, right| left.timestamp_unix.total_cmp(&right.timestamp_unix));
            document
                .hits
                .sort_by(|left, right| left.timestamp_unix.total_cmp(&right.timestamp_unix));
            let mut packets = document.packets.into_iter().peekable();
            let mut hits = document.hits.into_iter().peekable();

            while packets.peek().is_some() || hits.peek().is_some() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let take_packet = match (packets.peek(), hits.peek()) {
                    (Some(packet), Some(hit)) => packet.timestamp_unix <= hit.timestamp_unix,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                if take_packet {
                    let packet = packets.next().expect("peeked packet must exist");
                    if send_export_packet(packet, packet_debug_mode, &sender)? {
                        packet_count += 1;
                    }
                } else {
                    let hit = hits.next().expect("peeked hit must exist");
                    sender
                        .send(export_hit_event(hit))
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok((hit_count, packet_count))
        })();

        let _ = sender.send(EngineEvent::CaptureStopped);
        match result {
            Ok((hit_count, packet_count)) => {
                let _ = sender.send(EngineEvent::Status(format!(
                    "JSON import complete: {packet_count} packets, {hit_count} hits"
                )));
            }
            Err(error) => {
                let _ = sender.send(EngineEvent::Error(format!("JSON import failed: {error}")));
            }
        }
    })
}

fn send_export_packet(
    packet: ExportPacket,
    packet_debug_mode: PacketDebugMode,
    sender: &Sender<EngineEvent>,
) -> Result<bool, String> {
    if packet_debug_mode == PacketDebugMode::Off {
        return Ok(false);
    }
    let declared_ids = parse_export_ids(&packet.declared_ids);
    let (payload, decoded_text) = if packet_debug_mode == PacketDebugMode::FullPayload
        && !packet.payload_hex.trim().is_empty()
    {
        let payload = hex::decode(&packet.payload_hex)
            .map_err(|error| format!("payload_hex 无效: {error}"))?;
        let decoded_text = decode_payload_text(&payload);
        (payload, decoded_text)
    } else {
        (Vec::new(), packet.decoded_text)
    };
    let has_abyss_event = !abyss_events_from_text(packet.timestamp_unix, &decoded_text).is_empty();
    if !should_keep_debug_packet(
        packet.parsed_hits,
        packet.note.contains("HP"),
        packet.note.contains("Object/path")
            || packet.note.contains("Net identity")
            || packet.note.contains("Net runtime"),
        has_abyss_event,
        packet_debug_mode,
    ) {
        return Ok(false);
    }
    let mut note = packet.note;
    if packet_debug_mode == PacketDebugMode::FullPayload {
        let evidence = find_declared_character_evidence(&payload);
        append_packet_note(
            &mut note,
            binary_payload_diagnostic(&payload, &packet.direction, &decoded_text, &evidence),
        );
    }
    let packet = PacketDebug {
        timestamp: packet.timestamp_unix,
        source: packet.source,
        destination: packet.destination,
        direction: packet.direction,
        payload_len: packet.payload_len,
        declared_ids,
        parsed_hits: packet.parsed_hits,
        note,
        payload_preview: packet.payload_preview,
        payload_hex: if packet_debug_mode == PacketDebugMode::FullPayload {
            packet.payload_hex
        } else {
            String::new()
        },
        decoded_text,
    };
    for event in abyss_events_from_text(packet.timestamp, &packet.decoded_text) {
        sender
            .send(EngineEvent::Abyss(event))
            .map_err(|error| error.to_string())?;
    }
    sender
        .send(EngineEvent::Packet(packet))
        .map_err(|error| error.to_string())?;
    Ok(true)
}

fn export_hit_event(hit: ExportHit) -> EngineEvent {
    EngineEvent::Hit(Hit {
        timestamp: hit.timestamp_unix,
        char_id: hit.char_id,
        char_name: hit.char_name,
        char_known: true,
        damage: hit.damage,
        byte_offset: 0,
        bit_shift: 0,
        char_source: "export_json".to_owned(),
        direction: hit.direction,
        target_hp_before: hit.target_hp_before,
        target_hp_after: hit.target_hp_after,
        target_max_hp: hit.target_max_hp,
        target_hp_percent: hit.target_hp_percent,
        target_id: hit.target_id,
        target_name: hit.target_name,
        target_context: hit.target_context,
        gameplay_effect_index: hit.gameplay_effect_index,
        gameplay_effect_name: hit.gameplay_effect_name,
        ability_name: hit.ability_name,
        damage_name: hit.damage_name.map(|name| normalize_damage_name(&name)),
        attack_type: hit.attack_type.map(|attack_type| {
            if attack_type == "QTE" {
                "环合".to_owned()
            } else if let Some(reaction_type) = attack_type.strip_prefix("QTE·") {
                format!("环合·{reaction_type}")
            } else {
                attack_type
            }
        }),
    })
}

fn parse_capture_export(text: &str) -> Result<CaptureExport, String> {
    serde_json::from_str(text)
        .or_else(|_| {
            let repaired = text
                .lines()
                .map(|line| {
                    if line.trim_start().starts_with(r#""payload_hex":"#) && !line.ends_with(',') {
                        format!("{line},")
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::from_str(&repaired)
        })
        .map_err(|error| error.to_string())
}

fn parse_export_ids(value: &serde_json::Value) -> Vec<u32> {
    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .filter_map(serde_json::Value::as_u64)
            .filter_map(|value| u32::try_from(value).ok())
            .collect(),
        serde_json::Value::String(value) => value
            .trim_matches(['[', ']'])
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hit(timestamp: f64, before: f64, after: f64) -> Hit {
        Hit {
            timestamp,
            char_id: 1,
            char_name: "tester".to_owned(),
            char_known: true,
            damage: (before - after).abs().max(1.0),
            byte_offset: timestamp as usize,
            bit_shift: 0,
            char_source: "test".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: before,
            target_hp_after: after,
            target_max_hp: before.max(1.0),
            target_hp_percent: after / before.max(1.0) * 100.0,
            target_id: None,
            target_name: None,
            target_context: Vec::new(),
            gameplay_effect_index: None,
            gameplay_effect_name: None,
            ability_name: None,
            damage_name: None,
            attack_type: None,
        }
    }

    fn named_summary(hit: &Hit) -> TargetResolutionSummary {
        TargetResolutionSummary {
            target_id: hit.target_id.clone(),
            target_name: hit.target_name.clone(),
            target_context: hit.target_context.clone(),
            score: 120,
            confidence: TargetConfidence::Confirmed,
            direct_hp_evidence: true,
        }
    }

    #[test]
    fn json_export_without_payload_hex_imports_fast() {
        let packet = ExportPacket {
            timestamp_unix: 1.0,
            source: "1.1.1.1:1".to_owned(),
            destination: "2.2.2.2:2".to_owned(),
            direction: "C2S".to_owned(),
            payload_len: 128,
            declared_ids: serde_json::json!([1]),
            parsed_hits: 1,
            note: "hit packet".to_owned(),
            payload_preview: "abcd".to_owned(),
            payload_hex: "not-valid-hex".to_owned(),
            decoded_text: UNREADABLE_PROTOCOL_TEXT.to_owned(),
        };
        let (sender, receiver) = crossbeam_channel::unbounded();
        assert!(
            send_export_packet(packet, PacketDebugMode::Summary, &sender).expect("summary import")
        );
        let event = receiver.try_recv().expect("packet event");
        assert!(matches!(
            event,
            EngineEvent::Packet(PacketDebug {
                payload_hex,
                ..
            }) if payload_hex.is_empty()
        ));
    }

    #[test]
    fn final_target_id_never_uses_attribute_guid_when_no_track() {
        let mut hit = test_hit(1.0, 1000.0, 900.0);
        hit.target_id = Some("AttributeGuid:ABCDEF|path=WorldBoss_Boss13".to_owned());
        normalize_final_target_identity(&mut hit);
        assert_eq!(hit.target_id, None);
        assert!(hit.target_context.iter().any(|entry| {
            entry == "target_handle_candidate=AttributeGuid:ABCDEF|path=WorldBoss_Boss13"
        }));
    }

    #[test]
    fn unknown_hit_without_identity_candidates_records_decode_gap() {
        let mut hit = test_hit(1.0, 1000.0, 900.0);
        annotate_unknown_identity_gap(&mut hit, true, true, true);
        assert_eq!(hit.target_name, None);
        assert!(
            hit.target_context
                .iter()
                .any(|entry| entry == "target_unresolved=network_identity_not_decoded")
        );
        assert!(
            hit.target_context.iter().any(|entry| {
                entry == "target_identity_gap=actor_or_iris_config_id_not_decoded"
            })
        );
    }

    #[test]
    fn identity_gap_diagnostic_does_not_override_existing_resolution() {
        let mut named = test_hit(1.0, 1000.0, 900.0);
        named.target_name = Some("伤心英熊".to_owned());
        named
            .target_context
            .push("target_track_id=monster:mon_019#1".to_owned());
        annotate_unknown_identity_gap(&mut named, true, true, true);
        assert!(
            !named
                .target_context
                .iter()
                .any(|entry| entry == "target_unresolved=network_identity_not_decoded")
        );

        let mut ambiguous = test_hit(2.0, 1000.0, 900.0);
        ambiguous
            .target_context
            .push("target_unresolved=ambiguous_multi_target".to_owned());
        annotate_unknown_identity_gap(&mut ambiguous, true, true, true);
        assert!(
            !ambiguous.target_context.iter().any(|entry| {
                entry == "target_identity_gap=actor_or_iris_config_id_not_decoded"
            })
        );
    }

    #[test]
    fn payload_scan_reuses_shifted_buffers() {
        let payload = b"prefix /Game/Monster/Boss CurrentHP suffix";
        let scan = PayloadScan::new(payload);
        assert_eq!(scan.shifted.len(), 8);
        assert!(scan.contains_any_fast_hint());
        assert!(scan.decode_text().contains("/Game/Monster/Boss"));
        assert_eq!(scan.shifted.len(), 8);
    }

    #[test]
    fn pcap_import_does_not_clone_packet_data() {
        let path = std::env::temp_dir().join(format!(
            "nte_pcap_import_borrowed_{}.pcapng",
            std::process::id()
        ));
        {
            let file = File::create(&path).expect("create pcapng");
            let mut writer = PcapNgWriter::new(BufWriter::new(file)).expect("pcap writer");
            writer
                .write_pcapng_block(InterfaceDescriptionBlock::new(
                    DataLink::ETHERNET,
                    CAPTURE_SNAPLEN,
                ))
                .expect("write interface");
            let packet = [0_u8; 64];
            writer
                .write_pcapng_block(EnhancedPacketBlock {
                    interface_id: 0,
                    timestamp: Duration::from_secs(1),
                    original_len: packet.len() as u32,
                    data: Cow::Borrowed(&packet),
                    options: Vec::new(),
                })
                .expect("write packet");
        }

        let (sender, receiver) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = import_pcapng(
            path.clone(),
            Arc::new(HashMap::new()),
            None,
            false,
            PacketDebugMode::Off,
            sender,
            stop,
        );
        handle.join().expect("import thread");
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, EngineEvent::Packet(_)))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn packet_decoder_tracks_terminal_hit_before_death_cleanup() {
        let mut decoder = PacketDecoder::default();
        let mut first = test_hit(1.0, 1000.0, 100.0);
        first.target_name = Some("斑蝶".to_owned());
        first.target_context = vec![
            "target_path=/Game/Monster/Boss_Butterfly.Boss_Butterfly_C".to_owned(),
            "target_name=斑蝶".to_owned(),
            "target_name_resolution=table_resolved".to_owned(),
            "reason=hp_guid_timeline_match:test".to_owned(),
            "boss_hp_guid=abc".to_owned(),
        ];
        let mut first_summary = named_summary(&first);
        decoder.apply_target_track_attribution(
            &mut first,
            &mut first_summary,
            TrackPacketContext::default(),
        );

        let mut death = test_hit(1.5, 100.0, 0.0);
        let mut death_summary = TargetResolutionSummary::default();
        decoder.apply_target_track_attribution(
            &mut death,
            &mut death_summary,
            TrackPacketContext::default(),
        );
        decoder.clear_dead_target_handles_from_hit(&mut death);

        assert_eq!(death.target_name.as_deref(), Some("斑蝶"));
        assert!(
            death
                .target_context
                .iter()
                .any(|entry| entry.starts_with("target_track_id="))
        );
    }

    #[test]
    #[ignore]
    fn replay_capture_target_track_invariants() {
        let path = std::env::var("NTE_TEST_CAPTURE")
            .map(PathBuf::from)
            .expect("set NTE_TEST_CAPTURE to a pcapng path");
        let (sender, receiver) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = import_pcapng(
            path,
            Arc::new(HashMap::new()),
            None,
            false,
            PacketDebugMode::Off,
            sender,
            stop,
        );
        let mut hits = Vec::new();
        while let Ok(event) = receiver.recv_timeout(Duration::from_secs(30)) {
            match event {
                EngineEvent::Hit(hit) => hits.push(hit),
                EngineEvent::CaptureStopped => break,
                EngineEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        handle.join().expect("import thread should finish");

        let mut names_by_track = HashMap::<String, HashSet<String>>::new();
        let mut projected = 0usize;
        let mut ambiguous_unknown = 0usize;
        let mut terminal_named = 0usize;
        let mut terminal_names = HashSet::<String>::new();
        let mut dot_named = 0usize;
        let mut dot_unknown_not_ambiguous = 0usize;
        let mut dot_unknown_samples = Vec::new();
        for hit in &hits {
            if hit
                .target_context
                .iter()
                .any(|entry| entry == "target_name_resolution=track_continuity_projected")
            {
                projected += 1;
            }
            if hit.target_name.is_none()
                && hit
                    .target_context
                    .iter()
                    .any(|entry| entry == "target_unresolved=ambiguous_multi_target")
            {
                ambiguous_unknown += 1;
            }
            if hit.target_hp_after <= 1.0 && hit.target_name.is_some() {
                terminal_named += 1;
                if let Some(name) = hit.target_name.as_deref() {
                    terminal_names.insert(name.to_owned());
                }
            }
            let is_requiem_bleed = hit
                .damage_name
                .as_deref()
                .is_some_and(|name| name.contains("安魂曲流血伤害"));
            if is_requiem_bleed {
                if hit.target_name.is_some() {
                    dot_named += 1;
                } else if !hit
                    .target_context
                    .iter()
                    .any(|entry| entry == "target_unresolved=ambiguous_multi_target")
                {
                    dot_unknown_not_ambiguous += 1;
                    if dot_unknown_samples.len() < 5 {
                        dot_unknown_samples.push(format!(
                            "t={:.3} hp={:.0}->{:.0}/{:.0} ctx={}",
                            hit.timestamp,
                            hit.target_hp_before,
                            hit.target_hp_after,
                            hit.target_max_hp,
                            hit.target_context
                                .iter()
                                .filter(|entry| {
                                    entry.starts_with("track_")
                                        || entry.starts_with("target_unresolved=")
                                        || entry.starts_with("reason=")
                                })
                                .take(8)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(" | ")
                        ));
                    }
                }
            }
            let Some(track_id) = target_context_value(&hit.target_context, "target_track_id")
            else {
                continue;
            };
            let generation =
                target_context_value(&hit.target_context, "target_generation").unwrap_or("unknown");
            if let Some(name) = hit.target_name.as_deref() {
                names_by_track
                    .entry(format!("{track_id}#{generation}"))
                    .or_default()
                    .insert(name.to_owned());
            }
        }
        let conflicting_tracks = names_by_track
            .iter()
            .filter(|(_, names)| names.len() > 1)
            .collect::<Vec<_>>();
        println!(
            "replay target track stats: hits={} projected={} terminal_named={} terminal_names={:?} ambiguous_unknown={} dot_named={} dot_unknown_not_ambiguous={} dot_unknown_samples={:?} named_tracks={} names_by_track={:?}",
            hits.len(),
            projected,
            terminal_named,
            terminal_names,
            ambiguous_unknown,
            dot_named,
            dot_unknown_not_ambiguous,
            dot_unknown_samples,
            names_by_track.len(),
            names_by_track
        );
        assert!(
            conflicting_tracks.is_empty(),
            "same target_track_id/generation had multiple names: {conflicting_tracks:?}"
        );
        assert_eq!(
            dot_unknown_not_ambiguous, 0,
            "DOT hits should either project to a track or carry ambiguous context"
        );
    }

    #[test]
    #[ignore]
    fn dump_replay_target_diagnostics() {
        let path = std::env::var("NTE_TEST_CAPTURE")
            .map(PathBuf::from)
            .expect("set NTE_TEST_CAPTURE to a pcapng path");
        let (sender, receiver) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = import_pcapng(
            path,
            Arc::new(HashMap::new()),
            None,
            false,
            PacketDebugMode::Summary,
            sender,
            stop,
        );
        let mut hits = Vec::new();
        let mut packet_notes = Vec::new();
        while let Ok(event) = receiver.recv_timeout(Duration::from_secs(30)) {
            match event {
                EngineEvent::Hit(hit) => hits.push(hit),
                EngineEvent::Packet(packet) => {
                    if packet.parsed_hits > 0 || packet.note.contains("Object/path candidates") {
                        packet_notes.push(packet.note);
                    }
                }
                EngineEvent::CaptureStopped => break,
                EngineEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        handle.join().expect("import thread should finish");

        eprintln!("diagnostic hits={}", hits.len());
        for hit in hits.iter().take(120) {
            eprintln!(
                "hit t={:.3} dmg={:.0} hp={:.0}->{:.0}/{:.0} target={:?} id={:?} ge={:?} dmg_name={:?} ctx={}",
                hit.timestamp,
                hit.damage,
                hit.target_hp_before,
                hit.target_hp_after,
                hit.target_max_hp,
                hit.target_name,
                hit.target_id,
                hit.gameplay_effect_name,
                hit.damage_name,
                hit.target_context.join(" | ")
            );
        }
        eprintln!("diagnostic packets={}", packet_notes.len());
        for note in packet_notes.iter().take(120) {
            eprintln!("packet {note}");
        }
    }
}
