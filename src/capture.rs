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
};
use crate::net_event::{
    NetRuntimeAction, NetRuntimeEvent, NetRuntimeEventKind, NetRuntimeScanOptions,
    extract_net_runtime_events,
};
use crate::net_identity::{NetIdentityCandidateKind, extract_net_identity_candidates};
use crate::object_state::{ObjectHandleKind, ObjectStateStore, is_ignored_non_target_path};
use crate::parser::{
    GAMEPLAY_EFFECT_MAPPING_PATH, GameplayEffectSkill, ParsedGameplayEffect,
    SKILL_DAMAGE_DATA_PATH, WOODEN_DAMAGE_DESCRIPTIONS_PATH, classify_attack_type,
    classify_attack_type_from_description, declared_character_ids_from_evidence, find_data_file,
    find_declared_character_evidence, load_gameplay_effect_mapping, load_gameplay_effect_skills,
    load_wooden_damage_names, normalize_damage_name, parse_boss_hp_updates,
    parse_current_hp_updates, parse_damage_payload, parse_gameplay_effects, qte_reaction_type,
};

use crate::protocol::{TransportPacket, parse_single_bunch, parse_transport_packet};
use crate::resource_index::ResourceIndex;
use crate::runtime_mapping::{
    RuntimeMappingAction, RuntimeMappingEvent, RuntimeMappingTimeline,
    find_companion_runtime_mapping_sidecar, load_runtime_mapping_sidecar,
};
use crate::target_instance::{TargetAlias, TargetAliasKind, TargetInstanceStore};
use crate::target_resolver::{TargetConfidence, TargetResolutionSummary, TargetResolver};
use crate::ue_bitstream::extract_path_candidates;

const PCAP_ERRBUF_SIZE: usize = 256;
const MIN_READABLE_TEXT_LEN: usize = 4;
const MAX_IGNORABLE_BINARY_PACKET_LEN: usize = 96;
const UNREADABLE_PROTOCOL_TEXT: &str = "未解析到可读协议文本";
const CAPTURE_SNAPLEN: u32 = 65_535;
const RAW_CAPTURE_FLUSH_INTERVAL: u64 = 256;

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
    raw_capture: RawCaptureBuffer,
}

impl CaptureHandle {
    pub fn raw_capture(&self) -> RawCaptureBuffer {
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
    path: PathBuf,
    writer: Option<RawCaptureWriter>,
    packet_count: u64,
    captured_bytes: u64,
    write_error: Option<String>,
}

impl RawCaptureBuffer {
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
                path,
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
            .map(|capture| capture.path.clone())
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
        if path != capture.path {
            std::fs::copy(&capture.path, path).map_err(|error| {
                format!(
                    "failed to copy raw capture {} to {}: {error}",
                    capture.path.display(),
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
    let mut found = Vec::<(usize, String)>::new();
    for bit_shift in 0..8 {
        let shifted = decode_shifted_payload(data, bit_shift);
        for (score, value) in extract_length_prefixed_identifiers(&shifted) {
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

fn is_padding_payload(data: &[u8]) -> bool {
    data.is_empty()
        || data
            .first()
            .is_some_and(|first| data.iter().all(|byte| byte == first))
}

fn should_keep_debug_packet(
    payload: &[u8],
    declared_ids: &[u32],
    parsed_hits: usize,
    decoded_text: &str,
) -> bool {
    if parsed_hits > 0 || !declared_ids.is_empty() || decoded_text != UNREADABLE_PROTOCOL_TEXT {
        return true;
    }
    !is_padding_payload(payload) && payload.len() > MAX_IGNORABLE_BINARY_PACKET_LEN
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
    resource_index: ResourceIndex,
    target_resolver: TargetResolver,
    recent_target_hits: VecDeque<RecentTargetHit>,
    target_handle_aliases: HashMap<String, HandleTargetAlias>,
    follow_up_damage: FollowUpDamageTracker,
    character_declarations: HashMap<u32, f64>,
    resource_warnings: Vec<String>,
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
            resource_index: ResourceIndex::load_default_with_warnings(&mut resource_warnings),
            target_resolver: TargetResolver,
            recent_target_hits: VecDeque::new(),
            target_handle_aliases: HashMap::new(),
            follow_up_damage: FollowUpDamageTracker::default(),
            character_declarations: HashMap::new(),
            resource_warnings,
        }
    }
}

fn target_update_from_hit(hit: &Hit, summary: &TargetResolutionSummary) -> HitTargetUpdate {
    HitTargetUpdate {
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
    }
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
        self.register_target_handle_alias(&summary, hit.timestamp);
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
                for (target_id, alias) in
                    target_handle_aliases_from_summary(&resolved_summary, recent.hit.timestamp)
                {
                    merge_target_handle_alias(&mut self.target_handle_aliases, target_id, alias);
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
                    self.target_handle_aliases.remove(&alias.key());
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

    fn apply_net_runtime_event(&mut self, timestamp: f64, event: &NetRuntimeEvent) -> bool {
        let mut applied = false;
        if event.kind == NetRuntimeEventKind::SdkTargetData
            && let (Some(target_token), Some(current_hp)) =
                (event.target_token.as_deref(), event.current_hp)
        {
            self.object_state.observe_net_target_hp_update(
                timestamp,
                "sdk_target",
                target_token,
                current_hp as f64,
                None,
                format!("sdk_target_data:{}", event.evidence),
            );
            if let Some(instance_id) = self.target_instances.observe_sdk_net_target(
                timestamp,
                target_token,
                current_hp as f64,
                format!("sdk_target_data:{}", event.evidence),
            ) {
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
    if recent.hit.target_name.is_some() {
        return false;
    }
    let Some(alias) = target_alias_lookup_keys(
        recent
            .hit
            .target_id
            .as_ref()
            .or(recent.summary.target_id.as_ref()),
        &recent.hit.target_context,
    )
    .into_iter()
    .find_map(|key| aliases.get(&key)) else {
        return false;
    };
    apply_alias_to_hit(&mut recent.hit, alias);
    recent.summary.target_name = recent.hit.target_name.clone();
    recent.summary.target_context = recent.hit.target_context.clone();
    true
}

fn apply_handle_alias_to_hit_summary(
    hit: &mut Hit,
    summary: &mut TargetResolutionSummary,
    aliases: &HashMap<String, HandleTargetAlias>,
) -> bool {
    if hit.target_name.is_some() {
        return false;
    }
    let Some(alias) = target_alias_lookup_keys(
        hit.target_id.as_ref().or(summary.target_id.as_ref()),
        &hit.target_context,
    )
    .into_iter()
    .find_map(|key| aliases.get(&key)) else {
        return false;
    };
    apply_alias_to_hit(hit, alias);
    summary.target_name = hit.target_name.clone();
    summary.target_context = hit.target_context.clone();
    true
}

fn apply_alias_to_hit(hit: &mut Hit, alias: &HandleTargetAlias) {
    hit.target_name = Some(alias.target_name.clone());
    replace_target_path_context(&mut hit.target_context, &alias.target_path);
    push_unique_context(
        &mut hit.target_context,
        format!("target_name={}", alias.target_name),
    );
    push_unique_context(
        &mut hit.target_context,
        "target_name_resolution=handle_alias_applied".to_owned(),
    );
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

fn target_context_value<'a>(context: &'a [String], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    context
        .iter()
        .find_map(|value| value.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "None")
}

fn target_context_values<'a>(context: &'a [String], key: &str) -> impl Iterator<Item = &'a str> {
    let prefix = format!("{key}=");
    context
        .iter()
        .filter_map(move |value| value.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "None")
}

fn push_unique_context(context: &mut Vec<String>, value: String) {
    if !context.iter().any(|entry| entry == &value) {
        context.push(value);
    }
}

fn target_alias_lookup_keys(target_id: Option<&String>, context: &[String]) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(target_id) = target_id {
        extend_equivalent_target_alias_keys(&mut keys, target_id);
    }
    for value in target_context_values(context, "target_handle_candidate") {
        extend_equivalent_target_alias_keys(&mut keys, value);
    }
    for value in target_context_values(context, "boss_hp_guid") {
        extend_equivalent_target_alias_keys(&mut keys, &format!("boss_hp_guid:{value}"));
    }
    for value in target_context_values(context, "current_hp_token") {
        extend_equivalent_target_alias_keys(&mut keys, &format!("current_hp_token:{value}"));
    }
    for key in [
        "actor_channel",
        "iris_ref32",
        "netguid32",
        "netguid_packed",
        "sdk_net_target",
    ] {
        for value in target_context_values(context, key) {
            extend_equivalent_target_alias_keys(&mut keys, &format!("{key}:{value}"));
        }
    }
    keys
}

fn extend_equivalent_target_alias_keys(keys: &mut Vec<String>, key: &str) {
    for key in equivalent_target_alias_keys(key) {
        if !keys.iter().any(|existing| existing == &key) {
            keys.push(key);
        }
    }
}

fn equivalent_target_alias_keys(key: &str) -> Vec<String> {
    let key = normalize_target_alias_key(key);
    let mut keys = vec![key.clone()];
    if let Some(value) = key.strip_prefix("AttributeGuid:") {
        keys.push(format!("boss_hp_guid:{value}"));
    } else if let Some(value) = key.strip_prefix("boss_hp_guid:") {
        keys.push(format!("AttributeGuid:{value}"));
    } else if let Some(value) = key.strip_prefix("NetRefHandleCandidate:currenthp:") {
        keys.push(format!("current_hp_token:{value}"));
    } else if let Some(value) = key.strip_prefix("current_hp_token:") {
        keys.push(format!("NetRefHandleCandidate:currenthp:{value}"));
    } else if let Some(value) = key.strip_prefix("NetRefHandleCandidate:sdk_target:") {
        keys.push(format!("sdk_net_target:{value}"));
    } else if let Some(value) = key.strip_prefix("sdk_net_target:") {
        keys.push(format!("NetRefHandleCandidate:sdk_target:{value}"));
    } else if let Some(value) = key.strip_prefix("NetRefHandleCandidate:") {
        keys.push(format!("iris_ref32:{value}"));
    } else if let Some(value) = key.strip_prefix("iris_ref32:") {
        keys.push(format!("NetRefHandleCandidate:{value}"));
    } else if let Some(value) = key.strip_prefix("NetGuidCandidate:") {
        keys.push(format!("netguid32:{value}"));
        keys.push(format!("netguid_packed:{value}"));
    } else if let Some(value) = key.strip_prefix("netguid32:") {
        keys.push(format!("NetGuidCandidate:{value}"));
        keys.push(format!("netguid_packed:{value}"));
    } else if let Some(value) = key.strip_prefix("netguid_packed:") {
        keys.push(format!("NetGuidCandidate:{value}"));
        keys.push(format!("netguid32:{value}"));
    }
    keys
}

fn normalize_target_alias_key(key: &str) -> String {
    let key = key.trim();
    let Some((kind, value)) = key.split_once(':') else {
        return key.to_owned();
    };
    format!("{kind}:{}", value.trim().to_ascii_lowercase())
}

fn should_apply_target_update(
    old: &TargetResolutionSummary,
    new: &TargetResolutionSummary,
) -> bool {
    if old.target_id == new.target_id
        && new.target_id.is_some()
        && old.target_name.is_none()
        && new.target_name.is_some()
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

        let decoded_text = decode_payload_text(payload);
        let path_candidates = extract_path_candidates(payload);
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
        let evidence = find_declared_character_evidence(payload);
        let ids = declared_character_ids_from_evidence(&evidence);
        self.follow_up_damage
            .observe_characters(ids.iter().copied(), characters);
        let outgoing = infer_outgoing(src, src_port, dst, local_ip, &ids, &self.client_endpoints);
        if outgoing && !ids.is_empty() {
            self.client_endpoints.insert((src, src_port));
        }
        let direction = if outgoing { "C2S" } else { "S2C" };
        let gameplay_effects = parse_gameplay_effects(payload);
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
        let targetish_path_candidates = path_candidates
            .iter()
            .filter(|candidate| {
                self.resource_index
                    .resolved_name_for_path(&candidate.value)
                    .is_some()
                    || crate::object_state::is_targetish_path(&candidate.value)
            })
            .collect::<Vec<_>>();
        let transport_packet = parse_transport_packet(payload);
        let single_bunch = match &transport_packet {
            Some(TransportPacket::Sequenced(packet)) => parse_single_bunch(packet),
            _ => None,
        };
        let sdk_target_scan_enabled = !outgoing
            && (!targetish_path_candidates.is_empty()
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
        let mut hits = if outgoing {
            let packet_char_id = if ids.len() == 1 {
                ids.first().copied()
            } else {
                None
            };
            let session_key = (src, src_port, dst, dst_port);
            if let Some(id) = packet_char_id {
                self.session_characters.insert(session_key, id);
            }
            let fallback = self.session_characters.get(&session_key).copied();
            parse_damage_payload(
                payload,
                timestamp,
                packet_char_id,
                fallback,
                characters,
                &evidence,
            )
        } else {
            Vec::new()
        };
        let mut hit_summaries = Vec::with_capacity(hits.len());
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
                self.register_target_handle_alias(&summary, hit.timestamp);
                apply_handle_alias_to_hit_summary(hit, &mut summary, &self.target_handle_aliases);
                self.register_target_handle_alias(&summary, hit.timestamp);
            }
            hit_summaries.push(summary);
        }
        for (hit, summary) in hits.iter_mut().zip(&mut hit_summaries) {
            if hit.direction != "incoming" {
                apply_handle_alias_to_hit_summary(hit, summary, &self.target_handle_aliases);
            }
        }
        for character_id in &ids {
            self.character_declarations.insert(*character_id, timestamp);
        }
        self.character_declarations
            .retain(|_, declared_at| timestamp - *declared_at <= 10.0);
        let accepted = hits
            .iter()
            .filter(|hit| include_incoming || hit.direction != "incoming")
            .count();
        let preview_len = payload.len().min(96);
        let payload_hex = hex::encode(payload);
        for update in &current_hp_updates {
            let Some(target_token) = current_hp_target_token(&update.target_hint) else {
                continue;
            };
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
                let token = hex::encode(target_token);
                self.register_target_instance_alias_keys(
                    [
                        format!("current_hp_token:{token}"),
                        format!("NetRefHandleCandidate:currenthp:{token}"),
                    ],
                    &instance_id,
                    timestamp,
                );
            }
        }
        for update in &boss_hp_updates {
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
                let handle = hex::encode(update.target_handle);
                self.register_target_instance_alias_keys(
                    [
                        format!("boss_hp_guid:{handle}"),
                        format!("AttributeGuid:{handle}"),
                    ],
                    &instance_id,
                    timestamp,
                );
            }
        }
        if !current_hp_updates.is_empty()
            || !boss_hp_updates.is_empty()
            || !targetish_path_candidates.is_empty()
            || net_runtime_applied
        {
            self.backfill_recent_targets(timestamp, sender);
        }
        if current_hp_updates.is_empty()
            && boss_hp_updates.is_empty()
            && targetish_path_candidates.is_empty()
            && net_runtime_notes.is_empty()
            && !should_keep_debug_packet(payload, &ids, accepted, &decoded_text)
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
        let mut inferred_follow_up_hits = boss_hp_updates
            .iter()
            .filter_map(|update| {
                self.follow_up_damage
                    .observe_server_hp(timestamp, update.current_hp as f64)
            })
            .map(|mut hit| {
                let summary = self.target_resolver.apply_to_hit_with_summary(
                    &mut hit,
                    &self.object_state,
                    &self.target_instances,
                    &[],
                    &self.resource_index,
                );
                (hit, summary)
            })
            .collect::<Vec<_>>();
        for (hit, summary) in &mut inferred_follow_up_hits {
            self.register_target_handle_alias(summary, hit.timestamp);
            apply_handle_alias_to_hit_summary(hit, summary, &self.target_handle_aliases);
            self.register_target_handle_alias(summary, hit.timestamp);
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
                payload_preview: payload_hex[..preview_len * 2].to_owned(),
                payload_hex,
                decoded_text,
            },
        );
        for (hit, summary) in hits.into_iter().zip(hit_summaries) {
            if include_incoming || hit.direction != "incoming" {
                self.follow_up_damage
                    .observe_hit(&hit, hit.gameplay_effect_index, characters);
                self.remember_target_hit(&hit, summary);
                let _ = sender.send(EngineEvent::Hit(hit));
            }
        }
        for (hit, summary) in inferred_follow_up_hits {
            self.remember_target_hit(&hit, summary);
            let _ = sender.send(EngineEvent::Hit(hit));
        }
    }
}

pub fn start_capture(
    device: CaptureDevice,
    local_ip: Option<Ipv4Addr>,
    filter: String,
    include_incoming: bool,
    characters: Arc<HashMap<u32, CharacterInfo>>,
    sender: Sender<EngineEvent>,
) -> CaptureHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let raw_capture = RawCaptureBuffer::new(device.clone());
    let thread_raw_capture = raw_capture.clone();
    let thread = thread::spawn(move || {
        let result = run_capture(CaptureRunConfig {
            device: &device,
            local_ip,
            filter: &filter,
            include_incoming,
            characters: &characters,
            sender: &sender,
            stop: &thread_stop,
            raw_capture: &thread_raw_capture,
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

struct CaptureRunConfig<'a> {
    device: &'a CaptureDevice,
    local_ip: Option<Ipv4Addr>,
    filter: &'a str,
    include_incoming: bool,
    characters: &'a HashMap<u32, CharacterInfo>,
    sender: &'a Sender<EngineEvent>,
    stop: &'a AtomicBool,
    raw_capture: &'a RawCaptureBuffer,
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
            || "; raw capture unavailable".to_owned(),
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

        let mut decoder = PacketDecoder::default();
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
            let mut decoder = PacketDecoder::default();
            let mut runtime_mapping = load_companion_runtime_mapping(&path, &sender);
            if let Some(warning) = decoder.resource_warning() {
                let _ = sender.send(EngineEvent::Warning(warning));
            }
            let mut packet_count = 0;
            let mut supported_count = 0;

            while let Some(block) = reader.next_block() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let block = block.map_err(|error| error.to_string())?;
                let (interface_id, timestamp, data) = match block {
                    Block::EnhancedPacket(packet) => (
                        packet.interface_id as usize,
                        packet.timestamp.as_secs_f64(),
                        packet.data.into_owned(),
                    ),
                    Block::SimplePacket(packet) => (0, 0.0, packet.data.into_owned()),
                    _ => continue,
                };
                packet_count += 1;
                let Some(interface) = reader.interfaces().get(interface_id) else {
                    continue;
                };
                if interface.linktype != DataLink::ETHERNET {
                    continue;
                }
                supported_count += 1;
                runtime_mapping.align_to_packet_time(timestamp);
                decoder.apply_runtime_mapping_events(runtime_mapping.pop_due(timestamp), &sender);
                decoder.process_ethernet_frame(
                    &data,
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
                    if send_export_packet(packet, &sender)? {
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

fn send_export_packet(packet: ExportPacket, sender: &Sender<EngineEvent>) -> Result<bool, String> {
    let declared_ids = parse_export_ids(&packet.declared_ids);
    let payload = if packet.payload_hex.trim().is_empty() {
        Vec::new()
    } else {
        hex::decode(&packet.payload_hex).map_err(|error| format!("payload_hex 无效: {error}"))?
    };
    let decoded_text = if payload.is_empty() {
        packet.decoded_text
    } else {
        decode_payload_text(&payload)
    };
    if !should_keep_debug_packet(&payload, &declared_ids, packet.parsed_hits, &decoded_text) {
        return Ok(false);
    }
    let evidence = find_declared_character_evidence(&payload);
    let mut note = packet.note;
    append_packet_note(
        &mut note,
        binary_payload_diagnostic(&payload, &packet.direction, &decoded_text, &evidence),
    );
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
        payload_hex: packet.payload_hex,
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
    use crate::ue_bitstream::PathCandidate;
    use crossbeam_channel::unbounded;

    #[test]
    fn corrects_nanally_melee1_follow_up_from_server_total() {
        assert_eq!(
            corrected_follow_up_damage(3_850.0, 3_177.0, Some(241)),
            Some(641.0)
        );
        assert_eq!(
            corrected_follow_up_damage(3_417.0, 2_898.0, Some(241)),
            Some(569.0)
        );
        assert_eq!(
            corrected_follow_up_damage(2_115.0, 1_714.0, Some(145)),
            None
        );
    }

    #[test]
    fn follow_up_pending_hits_are_bounded_and_recent_hits_still_resolve() {
        let characters = HashMap::from([
            (
                1,
                CharacterInfo {
                    name_zh: "character1".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: None,
                    attribute: Some("灵".to_owned()),
                },
            ),
            (
                2,
                CharacterInfo {
                    name_zh: "character2".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: None,
                    attribute: Some("咒".to_owned()),
                },
            ),
        ]);
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_characters([1, 2], &characters);
        let mut hit = targetless_hit();
        hit.char_id = 1;
        hit.char_name = "character1".to_owned();
        hit.target_max_hp = 1_000_000.0;
        hit.target_hp_before = 1_000_000.0;
        hit.damage = 3_177.0;
        for index in 0..MAX_PENDING_FOLLOW_UP_HITS + 20 {
            hit.timestamp = index as f64 / 1_000.0;
            tracker.observe_hit(
                &hit,
                Some(NANALLY_MELEE1_GAMEPLAY_EFFECT_INDEX),
                &characters,
            );
        }
        assert_eq!(tracker.pending_hits.len(), MAX_PENDING_FOLLOW_UP_HITS);

        hit.timestamp = 2.0;
        tracker.observe_hit(
            &hit,
            Some(NANALLY_MELEE1_GAMEPLAY_EFFECT_INDEX),
            &characters,
        );
        assert_eq!(tracker.pending_hits.len(), 1);
        let follow_up = tracker
            .observe_server_hp(2.1, 996_150.0)
            .expect("recent pending hit should still resolve follow-up damage");
        assert_eq!(follow_up.damage, 641.0);
        assert_eq!(follow_up.char_name, "覆纹伤害");
        assert_eq!(follow_up.damage_name.as_deref(), Some("覆纹追加攻击"));
        assert_eq!(follow_up.attack_type.as_deref(), Some("覆纹"));
    }

    #[test]
    fn parses_export_ids_from_array_and_legacy_string() {
        assert_eq!(
            parse_export_ids(&serde_json::json!([1001, 1002])),
            [1001, 1002]
        );
        assert_eq!(
            parse_export_ids(&serde_json::json!("[1001, 1002]")),
            [1001, 1002]
        );
        assert!(parse_export_ids(&serde_json::json!([4294967296_u64])).is_empty());
    }

    #[test]
    fn send_export_packet_rejects_invalid_hex_payload() {
        let (sender, receiver) = unbounded();
        let packet = ExportPacket {
            timestamp_unix: 1.0,
            source: "127.0.0.1:1234".to_owned(),
            destination: "127.0.0.1:5678".to_owned(),
            direction: "S2C".to_owned(),
            payload_len: 0,
            declared_ids: serde_json::json!([1]),
            parsed_hits: 0,
            note: String::new(),
            payload_preview: String::new(),
            payload_hex: "ZZ".to_owned(),
            decoded_text: String::new(),
        };
        assert!(
            send_export_packet(packet, &sender)
                .unwrap_err()
                .contains("payload_hex 无效")
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn send_export_packet_accepts_empty_payload_hex() {
        let (sender, receiver) = unbounded();
        let packet = ExportPacket {
            timestamp_unix: 1.0,
            source: "127.0.0.1:1234".to_owned(),
            destination: "127.0.0.1:5678".to_owned(),
            direction: "S2C".to_owned(),
            payload_len: 0,
            declared_ids: serde_json::json!([1]),
            parsed_hits: 0,
            note: String::new(),
            payload_preview: String::new(),
            payload_hex: String::new(),
            decoded_text: "测试解码".to_owned(),
        };
        assert!(send_export_packet(packet, &sender).is_ok());
        match receiver.try_recv().expect("packet event should be emitted") {
            EngineEvent::Packet(packet) => {
                assert_eq!(packet.payload_hex, String::new());
                assert_eq!(packet.payload_len, 0);
            }
            _ => panic!("expected packet event"),
        }
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn backfills_unknown_hit_when_late_s2c_evidence_arrives() {
        let old = target_summary(None, 0, TargetConfidence::Unknown, false);
        let new = target_summary(
            Some("AttributeGuid:late".to_owned()),
            80,
            TargetConfidence::Probable,
            true,
        );
        assert!(should_apply_target_update(&old, &new));
    }

    #[test]
    fn does_not_downgrade_probable_target_with_weaker_late_context() {
        let old = target_summary(
            Some("AttributeGuid:target".to_owned()),
            70,
            TargetConfidence::Probable,
            true,
        );
        let new = target_summary(
            Some("PathOnly:/Game/Monster/target".to_owned()),
            40,
            TargetConfidence::Possible,
            false,
        );
        assert!(!should_apply_target_update(&old, &new));
    }

    #[test]
    fn does_not_replace_confirmed_target_with_different_non_direct_candidate() {
        let old = target_summary(
            Some("AttributeGuid:target-a".to_owned()),
            100,
            TargetConfidence::Confirmed,
            true,
        );
        let new = target_summary(
            Some("PathOnly:/Game/Monster/target-b".to_owned()),
            120,
            TargetConfidence::Confirmed,
            false,
        );
        assert!(!should_apply_target_update(&old, &new));
    }

    #[test]
    fn applies_follow_up_hit_target_resolution() {
        let old = target_summary(
            Some("AttributeGuid:target".to_owned()),
            70,
            TargetConfidence::Probable,
            true,
        );
        let new = target_summary(
            Some("AttributeGuid:target".to_owned()),
            95,
            TargetConfidence::Probable,
            true,
        );
        assert!(should_apply_target_update(&old, &new));
    }

    #[test]
    fn context_only_target_update_is_ignored() {
        let old = TargetResolutionSummary {
            target_id: Some("AttributeGuid:target".to_owned()),
            target_name: Some("Target".to_owned()),
            target_context: vec!["confidence=probable score=70".to_owned()],
            score: 70,
            confidence: TargetConfidence::Probable,
            direct_hp_evidence: true,
        };
        let new = TargetResolutionSummary {
            target_context: vec![
                "confidence=probable score=70".to_owned(),
                "reason=reordered_context".to_owned(),
            ],
            ..old.clone()
        };
        assert!(!should_apply_target_update(&old, &new));
    }

    #[test]
    fn same_target_id_update_can_fill_missing_name() {
        let old = TargetResolutionSummary {
            target_id: Some("AttributeGuid:target".to_owned()),
            target_name: None,
            target_context: vec!["confidence=confirmed score=100".to_owned()],
            score: 100,
            confidence: TargetConfidence::Confirmed,
            direct_hp_evidence: true,
        };
        let new = TargetResolutionSummary {
            target_name: Some("流梦种".to_owned()),
            target_context: vec![
                "confidence=probable score=70".to_owned(),
                "target_name=流梦种".to_owned(),
            ],
            score: 70,
            confidence: TargetConfidence::Probable,
            ..old.clone()
        };

        assert!(should_apply_target_update(&old, &new));
    }

    #[test]
    fn backfill_applies_table_resolved_name_below_probable_without_replacing_strong_id() {
        let (sender, receiver) = unbounded();
        let mut decoder = PacketDecoder::default();
        let mut hit = targetless_hit();
        hit.target_id = Some("AttributeGuid:strong".to_owned());
        let old_summary = TargetResolutionSummary {
            target_id: hit.target_id.clone(),
            target_name: None,
            target_context: vec!["confidence=confirmed score=100".to_owned()],
            score: 100,
            confidence: TargetConfidence::Confirmed,
            direct_hp_evidence: true,
        };
        decoder.recent_target_hits.push_back(RecentTargetHit {
            hit,
            summary: old_summary,
        });
        decoder.object_state.observe_path_candidate(
            0.2,
            &PathCandidate {
                value: "mon_04_BP".to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            },
            &decoder.resource_index,
        );

        decoder.backfill_recent_targets(0.2, &sender);

        let update = receiver
            .try_iter()
            .find_map(|event| match event {
                EngineEvent::HitTargetUpdate(update) => Some(update),
                _ => None,
            })
            .expect("table-resolved name should backfill even below probable");
        assert_eq!(update.target_name.as_deref(), Some("迷失种"));
        assert_eq!(update.target_id.as_deref(), Some("AttributeGuid:strong"));
    }

    #[test]
    fn backfill_applies_handle_alias_name_to_all_recent_hits_with_same_target_id() {
        let (sender, receiver) = unbounded();
        let mut decoder = PacketDecoder::default();
        let target_id = "AttributeGuid:c55c885903e84940936c7d0915dc8cad".to_owned();
        let mut early_hit = targetless_hit();
        early_hit.timestamp = 1.0;
        early_hit.target_id = Some(target_id.clone());
        let early_summary = TargetResolutionSummary {
            target_id: Some(target_id.clone()),
            target_name: None,
            target_context: vec!["confidence=confirmed score=100".to_owned()],
            score: 100,
            confidence: TargetConfidence::Confirmed,
            direct_hp_evidence: true,
        };
        decoder.recent_target_hits.push_back(RecentTargetHit {
            hit: early_hit,
            summary: early_summary,
        });
        decoder.target_handle_aliases.insert(
            target_id,
            HandleTargetAlias {
                target_path: "Boss_07_BP_WorldBoss".to_owned(),
                target_name: "塞润尼缇".to_owned(),
                source: "table_resolved_handle_alias".to_owned(),
                first_seen_at: 1.0_f64.to_bits(),
                last_seen_at: 1.0_f64.to_bits(),
            },
        );

        decoder.backfill_recent_targets(2.0, &sender);

        let update = receiver
            .try_iter()
            .find_map(|event| match event {
                EngineEvent::HitTargetUpdate(update) => Some(update),
                _ => None,
            })
            .expect("handle alias should backfill target name");
        assert_eq!(update.target_name.as_deref(), Some("塞润尼缇"));
        assert_eq!(
            update.target_id.as_deref(),
            Some("AttributeGuid:c55c885903e84940936c7d0915dc8cad")
        );
        assert!(
            update
                .target_context
                .iter()
                .any(|entry| entry == "target_path=Boss_07_BP_WorldBoss")
        );
        assert!(
            update
                .target_context
                .iter()
                .any(|entry| entry == "target_name_resolution=handle_alias_applied")
        );
    }

    #[test]
    fn handle_alias_backfill_names_weak_evidence_hits_with_same_target_id() {
        let (sender, receiver) = unbounded();
        let mut decoder = PacketDecoder::default();
        let target_id = "AttributeGuid:c55c885903e84940936c7d0915dc8cad".to_owned();

        let mut confirmed = targetless_hit();
        confirmed.timestamp = 1.0;
        confirmed.target_id = Some(target_id.clone());
        confirmed.target_name = Some("塞润尼缇".to_owned());
        confirmed.target_context = vec![
            "confidence=confirmed score=100".to_owned(),
            "reason=boss_hp_delta_match:3875".to_owned(),
            "target_path=Boss_07_BP_WorldBoss".to_owned(),
            "target_name=塞润尼缇".to_owned(),
        ];
        let confirmed_summary = TargetResolutionSummary {
            target_id: confirmed.target_id.clone(),
            target_name: confirmed.target_name.clone(),
            target_context: confirmed.target_context.clone(),
            score: 100,
            confidence: TargetConfidence::Confirmed,
            direct_hp_evidence: true,
        };
        decoder.remember_target_hit(&confirmed, confirmed_summary);

        for (timestamp, context) in [
            (
                2.0,
                vec![
                    "confidence=unknown score=5".to_owned(),
                    "reason=target_max_hp_only_weak".to_owned(),
                ],
            ),
            (
                3.0,
                vec![
                    "confidence=possible score=40".to_owned(),
                    "reason=single_high_confidence_target_window:35".to_owned(),
                    "reason=target_max_hp_only_weak".to_owned(),
                ],
            ),
        ] {
            let mut hit = targetless_hit();
            hit.timestamp = timestamp;
            hit.target_id = Some(target_id.clone());
            hit.target_context = context.clone();
            decoder.recent_target_hits.push_back(RecentTargetHit {
                hit,
                summary: TargetResolutionSummary {
                    target_id: Some(target_id.clone()),
                    target_name: None,
                    target_context: context,
                    score: 5,
                    confidence: TargetConfidence::Unknown,
                    direct_hp_evidence: false,
                },
            });
        }

        decoder.backfill_recent_targets(4.0, &sender);

        assert!(
            decoder
                .recent_target_hits
                .iter()
                .all(|recent| recent.hit.target_name.as_deref() == Some("塞润尼缇"))
        );
        let updates = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(
            updates
                .iter()
                .filter(|event| matches!(event, EngineEvent::HitTargetUpdate(_)))
                .count(),
            2
        );
        for recent in decoder.recent_target_hits.iter().skip(1) {
            assert!(
                recent
                    .hit
                    .target_context
                    .iter()
                    .any(|entry| entry == "target_name_resolution=handle_alias_applied")
            );
            assert!(
                recent
                    .hit
                    .target_context
                    .iter()
                    .any(|entry| entry == "target_path=Boss_07_BP_WorldBoss")
            );
        }
    }

    #[test]
    fn runtime_hp_alias_backfills_weak_hits_without_formal_target_id() {
        let (sender, receiver) = unbounded();
        let mut decoder = PacketDecoder::default();
        let boss18_handle = [
            0xd7, 0x3c, 0x42, 0x9e, 0xdf, 0x9f, 0x21, 0x48, 0x8f, 0x39, 0x25, 0x1e, 0xda, 0xd0,
            0x31, 0xea,
        ];
        let boss07_handle = [
            0xc5, 0x5c, 0x88, 0x59, 0x03, 0xe8, 0x49, 0x40, 0x93, 0x6c, 0x7d, 0x09, 0x15, 0xdc,
            0x8c, 0xad,
        ];
        let boss07_hex = hex::encode(boss07_handle);
        decoder.target_instances.observe_paths(
            0.0,
            &[PathCandidate {
                value: "boss_18_BP_DiyBoss".to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            }],
            &[],
            &decoder.resource_index,
        );
        let boss18_instance = decoder
            .target_instances
            .observe_boss_hp_guid(1.0, boss18_handle, 1_000_000.0, "boss18_hp".to_owned())
            .expect("boss18 hp should bind");
        decoder.register_target_instance_alias_keys(
            [
                format!("boss_hp_guid:{}", hex::encode(boss18_handle)),
                format!("AttributeGuid:{}", hex::encode(boss18_handle)),
            ],
            &boss18_instance,
            1.0,
        );
        decoder.target_instances.observe_paths(
            40.0,
            &[PathCandidate {
                value: "Boss_07_BP_WorldBoss".to_owned(),
                byte_offset: 0,
                bit_shift: 0,
                score: 240,
            }],
            &[],
            &decoder.resource_index,
        );
        assert!(
            decoder
                .target_instances
                .instance("Boss_07_BP_WorldBoss#1")
                .is_some(),
            "Boss07 pending instance was not created"
        );
        let boss07_instance = decoder
            .target_instances
            .observe_boss_hp_guid(49.0, boss07_handle, 973_097.0, "boss07_hp".to_owned())
            .expect("boss07 hp should bind despite boss18 active ttl");
        decoder.register_target_instance_alias_keys(
            [
                format!("boss_hp_guid:{boss07_hex}"),
                format!("AttributeGuid:{boss07_hex}"),
            ],
            &boss07_instance,
            49.0,
        );
        assert!(
            decoder
                .target_handle_aliases
                .get(&format!("AttributeGuid:{boss07_hex}"))
                .is_some_and(|alias| alias.target_name == "塞润尼缇"),
            "Boss07 handle alias should resolve to 塞润尼缇"
        );

        let target_id = format!("AttributeGuid:{boss07_hex}");
        let mut direct = targetless_hit();
        direct.timestamp = 49.1;
        direct.target_id = Some(target_id.clone());
        direct.target_context = vec![
            "confidence=confirmed score=100".to_owned(),
            "reason=boss_hp_delta_match:3875".to_owned(),
            format!("boss_hp_guid={boss07_hex}"),
        ];
        decoder.recent_target_hits.push_back(RecentTargetHit {
            hit: direct,
            summary: TargetResolutionSummary {
                target_id: Some(target_id),
                target_name: None,
                target_context: vec![format!("boss_hp_guid={boss07_hex}")],
                score: 100,
                confidence: TargetConfidence::Confirmed,
                direct_hp_evidence: true,
            },
        });

        for (timestamp, context) in [
            (
                49.2,
                vec![
                    "confidence=unknown score=5".to_owned(),
                    "reason=target_max_hp_only_weak".to_owned(),
                    format!("target_handle_candidate=AttributeGuid:{boss07_hex}"),
                    format!("boss_hp_guid={boss07_hex}"),
                ],
            ),
            (
                49.3,
                vec![
                    "confidence=possible score=40".to_owned(),
                    "reason=single_high_confidence_target_window:35".to_owned(),
                    "reason=target_max_hp_only_weak".to_owned(),
                    format!("target_handle_candidate=AttributeGuid:{boss07_hex}"),
                ],
            ),
        ] {
            let mut hit = targetless_hit();
            hit.timestamp = timestamp;
            hit.target_context = context.clone();
            decoder.recent_target_hits.push_back(RecentTargetHit {
                hit,
                summary: TargetResolutionSummary {
                    target_id: None,
                    target_name: None,
                    target_context: context,
                    score: 5,
                    confidence: TargetConfidence::Unknown,
                    direct_hp_evidence: false,
                },
            });
        }

        decoder.backfill_recent_targets(50.0, &sender);

        assert!(
            decoder
                .recent_target_hits
                .iter()
                .all(|recent| recent.hit.target_name.as_deref() == Some("塞润尼缇"))
        );
        assert_eq!(
            receiver
                .try_iter()
                .filter(|event| matches!(event, EngineEvent::HitTargetUpdate(_)))
                .count(),
            3
        );
        for recent in &decoder.recent_target_hits {
            assert!(
                recent
                    .hit
                    .target_context
                    .iter()
                    .any(|entry| entry == "target_path=Boss_07_BP_WorldBoss")
            );
        }
    }

    #[test]
    fn named_probable_candidate_replaces_internal_id_without_name() {
        let old = TargetResolutionSummary {
            target_id: Some("AttributeGuid:internal".to_owned()),
            target_name: None,
            target_context: vec!["confidence=confirmed score=120".to_owned()],
            score: 120,
            confidence: TargetConfidence::Confirmed,
            direct_hp_evidence: true,
        };
        let new = TargetResolutionSummary {
            target_id: Some("PathOnly:Boss_016_BP_DiyBoss_C_2147307033".to_owned()),
            target_name: Some("随心泥".to_owned()),
            target_context: vec![
                "confidence=probable score=60".to_owned(),
                "target_name=随心泥".to_owned(),
            ],
            score: 60,
            confidence: TargetConfidence::Probable,
            direct_hp_evidence: false,
        };

        assert!(should_apply_target_update(&old, &new));
    }

    #[test]
    fn local_ip_hint_controls_import_direction_inference() {
        let local_ip = Ipv4Addr::new(10, 0, 0, 2);
        let remote_ip = Ipv4Addr::new(10, 0, 0, 3);
        let endpoints = HashSet::new();

        assert!(infer_outgoing(
            local_ip,
            50_000,
            remote_ip,
            Some(local_ip),
            &[],
            &endpoints,
        ));
        assert!(!infer_outgoing(
            remote_ip,
            40_000,
            local_ip,
            Some(local_ip),
            &[1001],
            &endpoints,
        ));
        assert!(infer_outgoing(
            remote_ip,
            40_000,
            local_ip,
            None,
            &[1001],
            &endpoints,
        ));
    }

    fn target_summary(
        target_id: Option<String>,
        score: i32,
        confidence: TargetConfidence,
        direct_hp_evidence: bool,
    ) -> TargetResolutionSummary {
        TargetResolutionSummary {
            target_name: target_id.as_ref().map(|_| "Target".to_owned()),
            target_context: vec![format!("confidence={} score={score}", confidence.as_str())],
            target_id,
            score,
            confidence,
            direct_hp_evidence,
        }
    }

    fn targetless_hit() -> Hit {
        Hit {
            timestamp: 0.0,
            char_id: 1,
            char_name: "test character".to_owned(),
            char_known: true,
            damage: 100.0,
            byte_offset: 0,
            bit_shift: 0,
            char_source: "test".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: 0.0,
            target_hp_after: 0.0,
            target_max_hp: 0.0,
            target_hp_percent: 0.0,
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

    fn encoded_damage_record(damage: f32, target_hp_before: f32, target_max_hp: f32) -> Vec<u8> {
        let fields = [
            (12, damage.to_le_bytes().to_vec()),
            (12, target_hp_before.to_le_bytes().to_vec()),
            (12, target_max_hp.to_le_bytes().to_vec()),
            (13, 10.0_f64.to_le_bytes().to_vec()),
            (12, 10.0_f32.to_le_bytes().to_vec()),
            (12, damage.to_le_bytes().to_vec()),
            (6, 0_i32.to_le_bytes().to_vec()),
            (6, 0_i32.to_le_bytes().to_vec()),
            (6, 0_i32.to_le_bytes().to_vec()),
            (12, 0.0_f32.to_le_bytes().to_vec()),
        ];
        let mut encoded = Vec::new();
        for (field_type, value) in fields {
            encoded.push(field_type);
            encoded.extend_from_slice(&(value.len() as u32).to_le_bytes());
            encoded.extend_from_slice(&value);
        }
        encoded
    }

    fn boss_hp_update(handle: [u8; 16], hp: f32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0x06, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00]);
        payload.extend_from_slice(&handle);
        payload.extend_from_slice(&[0; 12]);
        payload.extend_from_slice(&hp.to_le_bytes());
        payload
    }

    fn current_hp_update(prefix: [u8; 16], hp: f32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&prefix);
        payload.extend_from_slice(&hp.to_le_bytes());
        payload
    }

    #[test]
    fn current_hp_token_uses_unfixed_prefix_bytes() {
        let prefix = [
            0x10, 0x00, 0x00, 0xe0, 0x4f, 0x33, 0x33, 0x44, 0x0f, 0x55, 0x66, 0x00, 0x00, 0x00,
            0x00, 0x24,
        ];

        assert_eq!(
            current_hp_target_token(&prefix),
            Some([0x10, 0x44, 0x55, 0x66])
        );

        let uninformative = [
            0x00, 0x00, 0x00, 0xe0, 0x4f, 0x33, 0x33, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x24,
        ];
        assert_eq!(current_hp_target_token(&uninformative), None);
    }

    fn ethernet_udp_frame(
        src: Ipv4Addr,
        src_port: u16,
        dst: Ipv4Addr,
        dst_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let udp_len = 8 + payload.len();
        let ip_len = 20 + udp_len;
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0, 1, 2, 3, 4, 5]);
        frame.extend_from_slice(&[6, 7, 8, 9, 10, 11]);
        frame.extend_from_slice(&0x0800_u16.to_be_bytes());
        frame.push(0x45);
        frame.push(0);
        frame.extend_from_slice(&(ip_len as u16).to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0, 0]);
        frame.push(64);
        frame.push(17);
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&src.octets());
        frame.extend_from_slice(&dst.octets());
        frame.extend_from_slice(&src_port.to_be_bytes());
        frame.extend_from_slice(&dst_port.to_be_bytes());
        frame.extend_from_slice(&(udp_len as u16).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn packet_decoder_attaches_target_context_from_hp_guid_and_path_evidence() {
        let (sender, receiver) = unbounded();
        let local = Ipv4Addr::new(10, 0, 0, 2);
        let remote = Ipv4Addr::new(10, 0, 0, 3);
        let characters = HashMap::from([(
            1003,
            CharacterInfo {
                name_zh: "早雾".to_owned(),
                name_en: "Sagiri".to_owned(),
                color: None,
                avatar: None,
                attribute: Some("咒".to_owned()),
            },
        )]);
        let handle = [0x21_u8; 16];
        let mut s2c_payload =
            b"/Game/Blueprints/Character/Monster/boss_07/BP_Boss_07.BP_Boss_07_C\0".to_vec();
        s2c_payload.extend_from_slice(&boss_hp_update(handle, 1000.0));
        s2c_payload.extend_from_slice(&boss_hp_update(handle, 900.0));
        let s2c = ethernet_udp_frame(remote, 30031, local, 50000, &s2c_payload);

        let mut c2s_payload = encoded_damage_record(100.0, 1000.0, 1_000_000.0);
        c2s_payload.extend_from_slice(&[5, 0, 0, 0, b'1', b'0', b'0', b'3', 0]);
        let c2s = ethernet_udp_frame(local, 50000, remote, 30031, &c2s_payload);

        let mut decoder = PacketDecoder::default();
        decoder.process_ethernet_frame(&s2c, 10.0, Some(local), true, &characters, &sender);
        decoder.process_ethernet_frame(&c2s, 10.1, Some(local), true, &characters, &sender);

        let events = receiver.try_iter().collect::<Vec<_>>();
        let hit = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::Hit(hit) => Some(hit),
                _ => None,
            })
            .expect("synthetic damage hit should be emitted");
        assert!(
            hit.target_id
                .as_deref()
                .is_some_and(|id| id.contains("AttributeGuid"))
        );
        assert!(
            hit.target_context
                .iter()
                .any(|entry| entry.contains("hp_guid_timeline_match"))
        );
        assert!(
            events.iter().any(|event| match event {
                EngineEvent::Packet(packet) => packet.note.contains("Object/path candidates"),
                _ => false,
            }),
            "debug packet should preserve path evidence"
        );
    }

    #[test]
    fn packet_decoder_backfills_target_when_s2c_evidence_arrives_after_hit() {
        let (sender, receiver) = unbounded();
        let local = Ipv4Addr::new(10, 0, 0, 2);
        let remote = Ipv4Addr::new(10, 0, 0, 3);
        let characters = HashMap::from([(
            1003,
            CharacterInfo {
                name_zh: "早雾".to_owned(),
                name_en: "Sagiri".to_owned(),
                color: None,
                avatar: None,
                attribute: Some("咒".to_owned()),
            },
        )]);
        let mut c2s_payload = encoded_damage_record(100.0, 1000.0, 1_000_000.0);
        c2s_payload.extend_from_slice(&[5, 0, 0, 0, b'1', b'0', b'0', b'3', 0]);
        let c2s = ethernet_udp_frame(local, 50000, remote, 30031, &c2s_payload);

        let handle = [0x22_u8; 16];
        let mut s2c_payload =
            b"/Game/Blueprints/Character/Monster/boss_07/BP_Boss_07.BP_Boss_07_C\0".to_vec();
        s2c_payload.extend_from_slice(&boss_hp_update(handle, 1000.0));
        s2c_payload.extend_from_slice(&boss_hp_update(handle, 900.0));
        let s2c = ethernet_udp_frame(remote, 30031, local, 50000, &s2c_payload);

        let mut decoder = PacketDecoder::default();
        decoder.process_ethernet_frame(&c2s, 10.0, Some(local), true, &characters, &sender);
        decoder.process_ethernet_frame(&s2c, 10.2, Some(local), true, &characters, &sender);

        let events = receiver.try_iter().collect::<Vec<_>>();
        let original_hit = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::Hit(hit) => Some(hit),
                _ => None,
            })
            .expect("hit should be emitted immediately");
        assert!(original_hit.target_name.is_none());
        let update = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::HitTargetUpdate(update) => Some(update),
                _ => None,
            })
            .expect("late S2C evidence should backfill target");
        assert!(
            update
                .target_id
                .as_deref()
                .is_some_and(|id| id.contains("AttributeGuid"))
        );
        assert_eq!(update.target_name.as_deref(), Some("塞润尼缇"));
        assert!(
            update
                .target_context
                .iter()
                .any(|entry| entry.contains("hp_guid_timeline_match"))
        );
    }

    #[test]
    fn packet_decoder_backfills_target_from_current_hp_token_timeline() {
        let (sender, receiver) = unbounded();
        let local = Ipv4Addr::new(10, 0, 0, 2);
        let remote = Ipv4Addr::new(10, 0, 0, 3);
        let characters = HashMap::from([(
            1003,
            CharacterInfo {
                name_zh: "早雾".to_owned(),
                name_en: "Sagiri".to_owned(),
                color: None,
                avatar: None,
                attribute: Some("咒".to_owned()),
            },
        )]);
        let prefix = [
            0x10, 0x00, 0x00, 0xe0, 0x4f, 0x33, 0x33, 0x44, 0x0f, 0x55, 0x66, 0x00, 0x00, 0x00,
            0x00, 0x24,
        ];
        let s2c_before = ethernet_udp_frame(
            remote,
            30031,
            local,
            50000,
            &current_hp_update(prefix, 1000.0),
        );
        let mut c2s_payload = encoded_damage_record(100.0, 1000.0, 1_000_000.0);
        c2s_payload.extend_from_slice(&[5, 0, 0, 0, b'1', b'0', b'0', b'3', 0]);
        let c2s = ethernet_udp_frame(local, 50000, remote, 30031, &c2s_payload);
        let s2c_after = ethernet_udp_frame(
            remote,
            30031,
            local,
            50000,
            &current_hp_update(prefix, 900.0),
        );

        let mut decoder = PacketDecoder::default();
        decoder.process_ethernet_frame(&s2c_before, 9.8, Some(local), true, &characters, &sender);
        decoder.process_ethernet_frame(&c2s, 10.0, Some(local), true, &characters, &sender);
        decoder.process_ethernet_frame(&s2c_after, 10.1, Some(local), true, &characters, &sender);

        let events = receiver.try_iter().collect::<Vec<_>>();
        let update = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::HitTargetUpdate(update) => Some(update),
                _ => None,
            })
            .expect("late CurrentHP token evidence should backfill target");
        assert!(
            update
                .target_id
                .as_deref()
                .is_some_and(|id| id.contains("NetRefHandleCandidate"))
        );
        assert!(update.target_context.iter().any(|entry| {
            entry.contains("net_target_hp_delta_match")
                || entry.contains("net_target_hp_timeline_match")
        }));
    }

    #[test]
    fn packet_decoder_backfills_old_hit_when_much_later_c2s_target_path_arrives() {
        let (sender, receiver) = unbounded();
        let local = Ipv4Addr::new(10, 0, 0, 2);
        let remote = Ipv4Addr::new(10, 0, 0, 3);
        let characters = HashMap::from([(
            1003,
            CharacterInfo {
                name_zh: "早雾".to_owned(),
                name_en: "Sagiri".to_owned(),
                color: None,
                avatar: None,
                attribute: Some("咒".to_owned()),
            },
        )]);
        let handle = [0x33_u8; 16];
        let s2c_before =
            ethernet_udp_frame(remote, 30031, local, 50000, &boss_hp_update(handle, 1000.0));
        let mut c2s_hit_payload = encoded_damage_record(100.0, 1000.0, 1_000_000.0);
        c2s_hit_payload.extend_from_slice(&[5, 0, 0, 0, b'1', b'0', b'0', b'3', 0]);
        let c2s_hit = ethernet_udp_frame(local, 50000, remote, 30031, &c2s_hit_payload);
        let s2c_after =
            ethernet_udp_frame(remote, 30031, local, 50000, &boss_hp_update(handle, 900.0));
        let s2c_later =
            ethernet_udp_frame(remote, 30031, local, 50000, &boss_hp_update(handle, 700.0));
        let c2s_path = ethernet_udp_frame(
            local,
            50000,
            remote,
            30031,
            b"mon_33_BP_World_Perform_C_2147325373\0",
        );

        let mut decoder = PacketDecoder::default();
        decoder.process_ethernet_frame(&s2c_before, 10.0, Some(local), true, &characters, &sender);
        decoder.process_ethernet_frame(&c2s_hit, 10.1, Some(local), true, &characters, &sender);
        decoder.process_ethernet_frame(&s2c_after, 10.2, Some(local), true, &characters, &sender);
        decoder.process_ethernet_frame(&s2c_later, 19.1, Some(local), true, &characters, &sender);
        decoder.process_ethernet_frame(&c2s_path, 19.2, Some(local), true, &characters, &sender);

        let events = receiver.try_iter().collect::<Vec<_>>();
        let update = events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::HitTargetUpdate(update) => Some(update),
                _ => None,
            })
            .find(|update| update.target_name.as_deref() == Some("流梦种"))
            .expect("late C2S target path should backfill the old hit name");
        assert!(
            update
                .target_context
                .iter()
                .any(|entry| entry == "target_path=mon_33_BP_World_Perform_C_2147325373")
        );
    }

    #[test]
    fn packet_decoder_loads_attack_resources_outside_project_cwd() {
        let decoder = PacketDecoder::default();

        assert!(
            decoder.resource_warnings.is_empty(),
            "{}",
            decoder.resource_warnings.join("; ")
        );
        assert_eq!(
            decoder.gameplay_effect_names.get(&241).map(String::as_str),
            Some("GE_Player_Nanally_Melee1_Damage")
        );
        assert_eq!(
            decoder
                .gameplay_effect_skills
                .get("GE_Player_Nanally_Melee1_Damage")
                .map(|skill| skill.attack_type.as_str()),
            Some("普攻")
        );
        assert_eq!(
            decoder
                .wooden_damage_names
                .get("GE_Player_Nanally_Melee1_Damage")
                .map(String::as_str),
            Some("娜娜莉普攻")
        );
    }

    #[test]
    fn resolves_missing_damage_name_from_unique_name_in_same_ability() {
        let skills = HashMap::from([
            (
                "GE_Player_Daffodill_Skill2_Damage".to_owned(),
                GameplayEffectSkill {
                    damage_source_category: Some("E".to_owned()),
                    ability_name: Some("GA_Daffodill_Skill".to_owned()),
                    attack_type: "E技能".to_owned(),
                },
            ),
            (
                "GE_Player_Daffodill_Skill6_Damage".to_owned(),
                GameplayEffectSkill {
                    damage_source_category: Some("E".to_owned()),
                    ability_name: Some("GA_Daffodill_Skill".to_owned()),
                    attack_type: "E技能".to_owned(),
                },
            ),
        ]);
        let names = HashMap::from([(
            "GE_Player_Daffodill_Skill2_Damage".to_owned(),
            "达芙蒂尔技能".to_owned(),
        )]);

        assert_eq!(
            resolve_damage_name("GE_Player_Daffodill_Skill6_Damage", &skills, &names).as_deref(),
            Some("达芙蒂尔技能")
        );
    }

    #[test]
    fn does_not_merge_ambiguous_names_from_same_ability() {
        let skills = HashMap::from([
            (
                "GE_Test_Skill1_Damage".to_owned(),
                GameplayEffectSkill {
                    damage_source_category: Some("E".to_owned()),
                    ability_name: Some("GA_Test_Skill".to_owned()),
                    attack_type: "E技能".to_owned(),
                },
            ),
            (
                "GE_Test_Skill2_Damage".to_owned(),
                GameplayEffectSkill {
                    damage_source_category: Some("E".to_owned()),
                    ability_name: Some("GA_Test_Skill".to_owned()),
                    attack_type: "E技能".to_owned(),
                },
            ),
            (
                "GE_Test_Skill3_Damage".to_owned(),
                GameplayEffectSkill {
                    damage_source_category: Some("E".to_owned()),
                    ability_name: Some("GA_Test_Skill".to_owned()),
                    attack_type: "E技能".to_owned(),
                },
            ),
        ]);
        let names = HashMap::from([
            ("GE_Test_Skill1_Damage".to_owned(), "测试技能一".to_owned()),
            ("GE_Test_Skill2_Damage".to_owned(), "测试技能二".to_owned()),
        ]);

        assert_eq!(
            resolve_damage_name("GE_Test_Skill3_Damage", &skills, &names),
            None
        );
    }
}
