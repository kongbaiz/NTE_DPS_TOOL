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
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use libloading::Library;
use pcap_file::DataLink;
use pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock;
use pcap_file::pcapng::blocks::interface_description::{
    InterfaceDescriptionBlock, InterfaceDescriptionOption,
};
use pcap_file::pcapng::{Block, PcapNgReader, PcapNgWriter};
use serde::Deserialize;

use crate::model::{
    AbyssEvent, AbyssHalf, CharacterInfo, EngineEvent, Hit, HitDamageCorrection, HitFollowUp,
    PacketDebug, TimeStopEvent,
};
use crate::parser::{
    GAMEPLAY_EFFECT_MAPPING_PATH, GameplayEffectSkill, ParsedGameplayEffect,
    SKILL_DAMAGE_DATA_PATH, ULTRA_TIME_STOP_DATA_PATH, UltraTimeStopEntry,
    WOODEN_DAMAGE_DESCRIPTIONS_PATH, classify_attack_type, classify_attack_type_from_description,
    declared_character_ids_from_evidence, find_data_file, find_declared_character_evidence,
    find_final_tower_character_evidence, load_gameplay_effect_mapping, load_gameplay_effect_skills,
    load_ultra_time_stops, load_wooden_damage_names, matches_shifted_bytes_at,
    normalize_damage_name, parse_boss_hp_updates, parse_current_hp_updates, parse_damage_payload,
    parse_gameplay_effects, qte_reaction_type,
};

use crate::protocol::{TransportPacket, parse_single_bunch, parse_transport_packet};

const PCAP_ERRBUF_SIZE: usize = 256;
const MIN_READABLE_TEXT_LEN: usize = 4;
const MAX_IGNORABLE_BINARY_PACKET_LEN: usize = 96;
const UNREADABLE_PROTOCOL_TEXT: &str = "未解析到可读协议文本";
const CAPTURE_SNAPLEN: u32 = 65_535;
const RAW_CAPTURE_FLUSH_INTERVAL: u64 = 256;
/// Bounded queue between the acquisition thread and the parser thread. Large enough that realistic
/// game traffic never fills it, so a parse latency spike no longer stalls `pcap_next_ex` (which
/// would let the Npcap kernel buffer overflow and drop frames). If it ever fills under pathological
/// load the frame is dropped for live parsing only — the raw frame is already written to the
/// PCAPNG and stays recoverable via Debug replay.
const CAPTURE_FRAME_QUEUE_CAPACITY: usize = 16_384;

struct CaptureFrame {
    data: Vec<u8>,
    timestamp: f64,
}

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

fn merged_character_evidence(
    declared: &[(u32, u8, usize)],
    final_tower: &[(u32, u8, usize)],
) -> Vec<(u32, u8, usize)> {
    let mut merged = declared.to_vec();
    for evidence in final_tower {
        if !merged.contains(evidence) {
            merged.push(*evidence);
        }
    }
    merged
}

fn append_unique_ids(ids: &mut Vec<u32>, new_ids: impl IntoIterator<Item = u32>) {
    for id in new_ids {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
}

fn character_ids_from_evidence_sources(
    declared: &[(u32, u8, usize)],
    final_tower: &[(u32, u8, usize)],
) -> Vec<u32> {
    declared_character_ids_from_evidence(&merged_character_evidence(declared, final_tower))
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
        "ft_character_",
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
    let mut seen = HashSet::new();
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
        if score > 0 && seen.insert(value.to_owned()) {
            found.push((score, value.to_owned()));
        }
    }
    found
}

fn decode_payload_text(data: &[u8]) -> String {
    let mut found = Vec::<(usize, String)>::new();
    let mut seen = HashSet::new();
    for bit_shift in 0..8 {
        let shifted = decode_shifted_payload(data, bit_shift);
        for (score, value) in extract_length_prefixed_identifiers(&shifted) {
            if seen.insert(value.clone()) {
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
            if score == 0 || !seen.insert(value.to_owned()) {
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
    for value in decoded_text.lines() {
        if let Some(stage) = parse_abyss_stage_id(value) {
            explicit_stage = Some(stage);
        }
    }
    if let Some((cycle, floor, half)) = explicit_stage {
        events.push(AbyssEvent::Stage {
            timestamp,
            cycle: Some(cycle),
            floor: Some(floor),
            half,
            allow_late_backfill: false,
        });
    } else if !is_restart
        && decoded_text.contains("FAbyssGamePlayData")
        && (is_success || !decoded_text.contains("AbyssClone"))
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
                allow_late_backfill: is_success,
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

fn fixed_ultra_time_stop_event(
    timestamp: f64,
    char_id: u32,
    ultra_time_stops: &HashMap<u32, UltraTimeStopEntry>,
    recent_ultra_time_stops: &mut HashMap<u32, f64>,
) -> Option<TimeStopEvent> {
    let entry = ultra_time_stops.get(&char_id)?;
    fixed_ultra_time_stop_event_with_duration(
        timestamp,
        char_id,
        &entry.ability_id,
        entry.end_ability_event_seconds,
        recent_ultra_time_stops,
    )
}

fn fixed_ultra_time_stop_event_with_duration(
    timestamp: f64,
    char_id: u32,
    ability_id: &str,
    duration: f64,
    recent_ultra_time_stops: &mut HashMap<u32, f64>,
) -> Option<TimeStopEvent> {
    if !duration.is_finite() || duration <= 0.0 {
        return None;
    }
    if recent_ultra_time_stops
        .get(&char_id)
        .is_some_and(|previous| timestamp - previous < duration.max(1.0))
    {
        return None;
    }
    recent_ultra_time_stops.insert(char_id, timestamp);
    recent_ultra_time_stops.retain(|_, previous| timestamp - *previous <= 30.0);
    Some(TimeStopEvent::UltraAnimation {
        timestamp,
        char_id,
        ability_id: ability_id.to_owned(),
        duration_seconds: duration,
    })
}

fn special_ultra_cooldown_events(
    timestamp: f64,
    decoded_text: &str,
    ultra_time_stops: &HashMap<u32, UltraTimeStopEntry>,
    recent_ultra_time_stops: &mut HashMap<u32, f64>,
) -> (Vec<TimeStopEvent>, bool) {
    let mut events = Vec::new();
    let mut handled_special_cooldown = false;
    for (char_id, entry) in ultra_time_stops {
        if entry
            .ignored_cooldown_tags
            .iter()
            .any(|tag| !tag.is_empty() && decoded_text.contains(tag))
        {
            handled_special_cooldown = true;
        }
        for cooldown in &entry.extra_cooldowns {
            if cooldown.cooldown_tag.is_empty() || !decoded_text.contains(&cooldown.cooldown_tag) {
                continue;
            }
            handled_special_cooldown = true;
            let ability_id = if cooldown.ability_id.is_empty() {
                entry.ability_id.as_str()
            } else {
                cooldown.ability_id.as_str()
            };
            if let Some(event) = fixed_ultra_time_stop_event_with_duration(
                timestamp,
                *char_id,
                ability_id,
                cooldown.duration_seconds,
                recent_ultra_time_stops,
            ) {
                events.push(event);
            }
        }
    }
    (events, handled_special_cooldown)
}

fn extra_time_stop_events_from_text(timestamp: f64, decoded_text: &str) -> Vec<TimeStopEvent> {
    let mut events = Vec::new();
    for line in decoded_text.lines() {
        if line.contains(JIN_ENTER_TIME_STOP_TAG) {
            events.push(TimeStopEvent::ExtraStart {
                timestamp,
                reason: JIN_EXTRA_TIME_STOP_REASON.to_owned(),
            });
        }
        if line.contains(JIN_CLEAR_TIME_STOP_TAG) {
            events.push(TimeStopEvent::ExtraEnd {
                timestamp,
                reason: JIN_EXTRA_TIME_STOP_REASON.to_owned(),
            });
        }
    }
    events
}

#[derive(Default)]
struct UltraTimeStopTracker {
    recent_emitted: HashMap<u32, f64>,
    pending_cooldowns: Vec<PendingUltraTimeStop>,
}

struct PendingUltraTimeStop {
    timestamp: f64,
}

impl UltraTimeStopTracker {
    fn events_from_packet(
        &mut self,
        timestamp: f64,
        decoded_text: &str,
        declared_ids: &[u32],
        ultra_time_stops: &HashMap<u32, UltraTimeStopEntry>,
    ) -> Vec<TimeStopEvent> {
        let mut events = extra_time_stop_events_from_text(timestamp, decoded_text);
        self.resolve_finished_cooldowns(timestamp, decoded_text, ultra_time_stops, &mut events);
        let (mut special_events, handled_special_cooldown) = special_ultra_cooldown_events(
            timestamp,
            decoded_text,
            ultra_time_stops,
            &mut self.recent_emitted,
        );
        events.append(&mut special_events);

        if handled_special_cooldown || !decoded_text.contains(ULTRA_COOLDOWN_TAG_PREFIX) {
            return events;
        }
        match declared_ids {
            [char_id] => {
                if let Some(event) = fixed_ultra_time_stop_event(
                    timestamp,
                    *char_id,
                    ultra_time_stops,
                    &mut self.recent_emitted,
                ) {
                    events.push(event);
                }
            }
            [] => {
                self.pending_cooldowns
                    .push(PendingUltraTimeStop { timestamp });
                events.push(TimeStopEvent::ExtraStart {
                    timestamp,
                    reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
                });
            }
            _ => {}
        }
        events
    }

    fn events_from_hits(
        &mut self,
        hits: &[Hit],
        ultra_time_stops: &HashMap<u32, UltraTimeStopEntry>,
    ) -> Vec<TimeStopEvent> {
        let mut events = Vec::new();
        for hit in hits {
            self.resolve_cooldown_from_hit(hit, ultra_time_stops, &mut events);
        }
        events
    }

    fn resolve_cooldown_from_hit(
        &mut self,
        hit: &Hit,
        ultra_time_stops: &HashMap<u32, UltraTimeStopEntry>,
        events: &mut Vec<TimeStopEvent>,
    ) {
        let Some(char_id) = ultra_damage_time_stop_char_id(hit, ultra_time_stops) else {
            return;
        };
        let Some((index, _)) = self
            .pending_cooldowns
            .iter()
            .enumerate()
            .filter(|(_, pending)| {
                hit.timestamp >= pending.timestamp
                    && hit.timestamp - pending.timestamp <= ULTRA_TIME_STOP_PENDING_WINDOW_SECONDS
            })
            .max_by(|(_, left), (_, right)| left.timestamp.total_cmp(&right.timestamp))
        else {
            return;
        };
        let pending = self.pending_cooldowns.swap_remove(index);
        if let Some(event) = fixed_ultra_time_stop_event(
            pending.timestamp,
            char_id,
            ultra_time_stops,
            &mut self.recent_emitted,
        ) {
            let TimeStopEvent::UltraAnimation {
                timestamp,
                duration_seconds,
                ..
            } = &event
            else {
                unreachable!("fixed ultra time stop always emits an ultra animation event");
            };
            events.push(TimeStopEvent::ExtraEnd {
                timestamp: timestamp + duration_seconds,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            });
            events.push(event);
        } else {
            events.push(TimeStopEvent::ExtraEnd {
                timestamp: pending.timestamp,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            });
        }
    }

    fn resolve_finished_cooldowns(
        &mut self,
        timestamp: f64,
        _decoded_text: &str,
        _ultra_time_stops: &HashMap<u32, UltraTimeStopEntry>,
        events: &mut Vec<TimeStopEvent>,
    ) {
        let mut index = 0;
        while index < self.pending_cooldowns.len() {
            let pending_timestamp = self.pending_cooldowns[index].timestamp;
            let expired = timestamp - pending_timestamp > ULTRA_TIME_STOP_PENDING_WINDOW_SECONDS;
            if !expired {
                index += 1;
                continue;
            }
            let pending = self.pending_cooldowns.swap_remove(index);
            events.push(TimeStopEvent::ExtraEnd {
                timestamp: pending.timestamp,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            });
        }
    }
}

fn ultra_damage_time_stop_char_id(
    hit: &Hit,
    ultra_time_stops: &HashMap<u32, UltraTimeStopEntry>,
) -> Option<u32> {
    if hit.direction == "incoming" {
        return None;
    }
    let ability_name = hit.ability_name.as_deref()?;
    let entry = ultra_time_stops.get(&hit.char_id)?;
    (ability_name == entry.ability_id && ability_name.contains("UltraSkill")).then_some(hit.char_id)
}

fn send_packet_events(sender: &Sender<EngineEvent>, packet: PacketDebug) {
    for event in abyss_events_from_text(packet.timestamp, &packet.decoded_text) {
        let _ = sender.send(EngineEvent::Abyss(event));
    }
    let _ = sender.send(EngineEvent::Packet(Box::new(packet)));
}

const MAX_PENDING_FOLLOW_UP_HITS: usize = 256;
const AMBIGUOUS_HIT_CONFIRMATION_WINDOW_SECONDS: f64 = 0.5;
const FUWEN_START_SIGNATURE_SHIFT: u8 = 3;
const FUWEN_START_SIGNATURE_OFFSET: usize = 22;
const FUWEN_START_SIGNATURE: &[u8] = &[1, 0, 0, 0, 2, 0, 0, 0];
const FUWEN_ENTERING_ID_SHIFT: u8 = 0;
const FUWEN_ENTERING_ID_OFFSET: usize = 53;
const FUWEN_PREVIOUS_ID_SHIFT: u8 = 2;
const FUWEN_PREVIOUS_ID_OFFSET: usize = 66;
const MIN_FOLLOW_UP_RESIDUAL_DAMAGE: f64 = 1.0;
/// How far back to look for the real owner of an attribute-locked reaction whose
/// damage packet carried no caster. Matches the 3s window used by the 环合 retag.
const REACTION_REATTRIBUTION_WINDOW_SECONDS: f64 = 3.0;
const RECENT_CONFIRMED_HIT_WINDOW_SECONDS: f64 = 0.75;
const UNTYPED_SHADOW_HIT_WINDOW_SECONDS: f64 = 0.05;
const BOSS_HP_SYNC_WINDOW_SECONDS: f64 = 1.0;
const SERVER_DAMAGE_CALIBRATION_WINDOW_SECONDS: f64 = 1.0;
const JIN_EXTRA_TIME_STOP_REASON: &str = "Event.Montage.Player.UltraSkill.Jin";
const JIN_ENTER_TIME_STOP_TAG: &str = "Event.Montage.Player.UltraSkill.Jin.EnterTimeStop";
const JIN_CLEAR_TIME_STOP_TAG: &str = "Event.Montage.Player.UltraSkill.Jin.ClearTimeStop";
const ULTRA_COOLDOWN_TAG_PREFIX: &str = "CoolDown.Player.UltraSkill";
const PENDING_ULTRA_TIME_STOP_REASON: &str = "CoolDown.Player.UltraSkill.Pending";
const ULTRA_TIME_STOP_PENDING_WINDOW_SECONDS: f64 = 4.5;

fn hit_can_trigger_fuwen_follow_up(hit: &Hit) -> bool {
    match hit.attack_type.as_deref() {
        Some("创生") | Some("创生花") | Some("覆纹") | Some("延滞") | Some("黯星")
        | Some("浊燃") | Some("浸染") | Some("盈蓄") | Some("失谐") => false,
        Some(attack_type) if attack_type.starts_with("环合·") => attack_type == "环合·覆纹",
        _ => true,
    }
}

#[derive(Clone)]
struct PendingHit {
    hit: Hit,
}

#[derive(Default)]
struct FollowUpDamageTracker {
    last_server_hp: Option<f64>,
    last_hit_timestamp: Option<f64>,
    target_max_hp: Option<f64>,
    pending_hits: VecDeque<PendingHit>,
    team_attributes: HashSet<String>,
    character_attributes: HashMap<u32, String>,
    fuwen_active: bool,
    fuwen_start_pending: bool,
    fuwen_recorded_damage: bool,
}

impl FollowUpDamageTracker {
    fn reset_battle(&mut self) {
        self.pending_hits.clear();
        self.team_attributes.clear();
        self.character_attributes.clear();
        self.clear_fuwen_state();
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
        _gameplay_effect_index: Option<u32>,
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
        self.pending_hits.push_back(PendingHit { hit: hit.clone() });
        while self.pending_hits.len() > MAX_PENDING_FOLLOW_UP_HITS {
            self.pending_hits.pop_front();
        }
    }

    fn observe_fuwen_start_candidate(
        &mut self,
        _timestamp: f64,
        entering_character_id: u32,
        previous_character_id: u32,
        characters: &HashMap<u32, CharacterInfo>,
    ) {
        self.observe_characters([entering_character_id, previous_character_id], characters);
        self.fuwen_start_pending = true;
    }

    fn observe_fuwen_trigger_hit(&mut self, hit: &Hit) {
        if hit.direction == "incoming" || hit.attack_type.as_deref() != Some("环合·覆纹") {
            return;
        }
        self.fuwen_active = true;
        self.fuwen_start_pending = false;
        self.fuwen_recorded_damage = false;
        self.pending_hits.clear();
        self.last_server_hp = None;
    }

    fn observe_server_hp(&mut self, timestamp: f64, current_hp: f64) -> Option<HitFollowUp> {
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
        let source = self.pending_hits.pop_front()?.hit;
        if !hit_can_trigger_fuwen_follow_up(&source) {
            return None;
        }
        let residual_damage = actual_damage - source.damage;
        let has_required_team_attributes =
            self.team_attributes.contains("灵") && self.team_attributes.contains("咒");
        let source_attribute = self.character_attributes.get(&source.char_id)?;
        if !has_required_team_attributes || !matches!(source_attribute.as_str(), "灵" | "咒") {
            return None;
        }
        if !self.fuwen_active {
            return None;
        }
        if residual_damage < MIN_FOLLOW_UP_RESIDUAL_DAMAGE {
            return None;
        }
        self.fuwen_recorded_damage = true;
        Some(HitFollowUp {
            source_timestamp: source.timestamp,
            source_char_id: source.char_id,
            source_damage: source.damage,
            source_target_hp_before: source.target_hp_before,
            source_target_hp_after: source.target_hp_after,
            source_target_max_hp: source.target_max_hp,
            source_gameplay_effect_index: source.gameplay_effect_index,
            timestamp,
            damage: residual_damage,
            target_hp_after: current_hp,
            target_hp_percent: if source.target_max_hp > 0.0 {
                current_hp / source.target_max_hp * 100.0
            } else {
                0.0
            },
            damage_name: Some("覆纹追加攻击".to_owned()),
            attack_type: Some("覆纹".to_owned()),
            damage_attribute: Some(source_attribute.clone()),
        })
    }

    fn clear_fuwen_state(&mut self) {
        self.fuwen_active = false;
        self.fuwen_start_pending = false;
        self.fuwen_recorded_damage = false;
    }
}

#[derive(Clone)]
struct ServerDamagePendingHit {
    hit: Hit,
}

#[derive(Clone, Copy)]
struct ServerHpSnapshot {
    timestamp: f64,
    hp: f64,
}

#[derive(Default)]
struct ServerDamageCalibrationTracker {
    hp_by_handle: HashMap<[u8; 16], ServerHpSnapshot>,
    pending_hits: VecDeque<ServerDamagePendingHit>,
}

impl ServerDamageCalibrationTracker {
    fn observe_hit(&mut self, hit: &Hit) {
        if hit.direction == "incoming"
            || hit.char_id == 0
            || hit.target_max_hp <= 0.0
            || hit.target_hp_before <= 0.0
        {
            return;
        }
        if self
            .pending_hits
            .back()
            .is_some_and(|pending| hit.timestamp - pending.hit.timestamp > 2.0)
        {
            self.pending_hits.clear();
        }
        self.pending_hits
            .push_back(ServerDamagePendingHit { hit: hit.clone() });
        while self.pending_hits.len() > MAX_PENDING_FOLLOW_UP_HITS {
            self.pending_hits.pop_front();
        }
    }

    fn observe_boss_hp(
        &mut self,
        timestamp: f64,
        update: &crate::parser::ParsedBossHpUpdate,
    ) -> Option<HitDamageCorrection> {
        let current_hp = if update.current_hp <= 1.0 {
            0.0
        } else {
            update.current_hp as f64
        };
        let previous = self.hp_by_handle.insert(
            update.target_handle,
            ServerHpSnapshot {
                timestamp,
                hp: current_hp,
            },
        );
        self.pending_hits.retain(|pending| {
            timestamp - pending.hit.timestamp <= SERVER_DAMAGE_CALIBRATION_WINDOW_SECONDS
        });
        let previous = previous?;
        if current_hp >= previous.hp {
            self.pending_hits
                .retain(|pending| pending.hit.timestamp > timestamp);
            return None;
        }
        let candidates = self
            .pending_hits
            .iter()
            .enumerate()
            .filter(|(_, pending)| {
                pending.hit.timestamp > previous.timestamp
                    && pending.hit.timestamp <= timestamp
                    && pending.hit.target_max_hp > 0.0
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return None;
        }
        let source_index = candidates[0];
        let source = self.pending_hits[source_index].hit.clone();
        self.pending_hits
            .retain(|pending| pending.hit.timestamp > source.timestamp);
        let damage = previous.hp - current_hp;
        if damage < MIN_FOLLOW_UP_RESIDUAL_DAMAGE {
            return None;
        }
        Some(HitDamageCorrection {
            source_timestamp: source.timestamp,
            source_char_id: source.char_id,
            source_damage: source.damage,
            source_target_hp_before: source.target_hp_before,
            source_target_hp_after: source.target_hp_after,
            source_target_max_hp: source.target_max_hp,
            source_gameplay_effect_index: source.gameplay_effect_index,
            damage,
            target_hp_before: previous.hp,
            target_hp_after: current_hp,
            target_hp_percent: if source.target_max_hp > 0.0 {
                current_hp / source.target_max_hp * 100.0
            } else {
                0.0
            },
        })
    }
}

struct PacketDecoder {
    session_characters: HashMap<(Ipv4Addr, u16, Ipv4Addr, u16), u32>,
    client_endpoints: HashSet<(Ipv4Addr, u16)>,
    gameplay_effect_names: HashMap<u32, String>,
    gameplay_effect_skills: HashMap<String, GameplayEffectSkill>,
    ultra_time_stops: HashMap<u32, UltraTimeStopEntry>,
    wooden_damage_names: HashMap<String, String>,
    follow_up_damage: FollowUpDamageTracker,
    server_damage_calibration: ServerDamageCalibrationTracker,
    use_server_damage_calibration: bool,
    character_declarations: HashMap<u32, f64>,
    ultra_time_stop: UltraTimeStopTracker,
    pending_ambiguous_hits: Vec<Hit>,
    recent_confirmed_hits: Vec<Hit>,
    resource_warnings: Vec<String>,
}

#[derive(Default)]
struct PreparedHits {
    emit: Vec<Hit>,
    filtered_incoming: usize,
    deferred_ambiguous: usize,
    suppressed_ambiguous: usize,
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
        let ultra_time_stops = load_resource(
            ULTRA_TIME_STOP_DATA_PATH,
            &mut resource_warnings,
            load_ultra_time_stops,
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
            ultra_time_stops,
            wooden_damage_names,
            follow_up_damage: FollowUpDamageTracker::default(),
            server_damage_calibration: ServerDamageCalibrationTracker::default(),
            use_server_damage_calibration: false,
            character_declarations: HashMap::new(),
            ultra_time_stop: UltraTimeStopTracker::default(),
            pending_ambiguous_hits: Vec::new(),
            recent_confirmed_hits: Vec::new(),
            resource_warnings,
        }
    }
}

impl PacketDecoder {
    fn with_server_damage_calibration(use_server_damage_calibration: bool) -> Self {
        Self {
            use_server_damage_calibration,
            ..Self::default()
        }
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

    fn take_expired_ambiguous_hits(&mut self, timestamp: f64) -> Vec<Hit> {
        let mut expired = Vec::new();
        let mut pending = Vec::with_capacity(self.pending_ambiguous_hits.len());
        for hit in self.pending_ambiguous_hits.drain(..) {
            if timestamp - hit.timestamp > AMBIGUOUS_HIT_CONFIRMATION_WINDOW_SECONDS {
                expired.push(hit);
            } else {
                pending.push(hit);
            }
        }
        self.pending_ambiguous_hits = pending;
        expired
    }

    fn take_all_ambiguous_hits(&mut self) -> Vec<Hit> {
        self.pending_ambiguous_hits.drain(..).collect()
    }

    fn emit_hits(
        &mut self,
        hits: impl IntoIterator<Item = Hit>,
        characters: &HashMap<u32, CharacterInfo>,
        sender: &Sender<EngineEvent>,
    ) {
        for hit in hits {
            self.follow_up_damage
                .observe_hit(&hit, hit.gameplay_effect_index, characters);
            if self.use_server_damage_calibration {
                self.server_damage_calibration.observe_hit(&hit);
            }
            let _ = sender.send(EngineEvent::Hit(Box::new(hit)));
        }
    }

    fn prepare_hits_for_emission(
        &mut self,
        hits: Vec<Hit>,
        declared_ids: &[u32],
        include_incoming: bool,
        characters: &HashMap<u32, CharacterInfo>,
    ) -> PreparedHits {
        let mut prepared = PreparedHits::default();
        for mut hit in hits {
            self.recent_confirmed_hits.retain(|confirmed| {
                hit.timestamp - confirmed.timestamp <= RECENT_CONFIRMED_HIT_WINDOW_SECONDS
            });
            if !include_incoming && hit.direction == "incoming" {
                prepared.filtered_incoming += 1;
                continue;
            }
            if gameplay_effect_confirms_session_hit(&hit, declared_ids, characters) {
                hit.direction = "outgoing".to_owned();
                hit.char_source = "gameplay_effect".to_owned();
            }
            if is_recent_confirmed_duplicate(&hit, &self.recent_confirmed_hits) {
                prepared.suppressed_ambiguous += 1;
                continue;
            }
            if is_ambiguous_session_hit(&hit, declared_ids) {
                self.pending_ambiguous_hits.push(hit);
                prepared.deferred_ambiguous += 1;
                continue;
            }
            prepared.suppressed_ambiguous += self.suppress_matching_ambiguous_hits(&hit);
            if is_confirmed_packet_hit(&hit) {
                self.recent_confirmed_hits.push(hit.clone());
            }
            prepared.emit.push(hit);
        }
        prepared
    }

    fn infer_boss_hp_sync_damage(
        &mut self,
        timestamp: f64,
        current_hp: f64,
        characters: &HashMap<u32, CharacterInfo>,
    ) -> Option<HitFollowUp> {
        self.recent_confirmed_hits
            .retain(|hit| timestamp - hit.timestamp <= BOSS_HP_SYNC_WINDOW_SECONDS);
        if current_hp > 1.0 {
            return None;
        }
        let current_hp = 0.0;
        if self
            .recent_confirmed_hits
            .iter()
            .any(|hit| nearly_same(hit.target_hp_after, current_hp))
        {
            return None;
        }
        let source_index = self.recent_confirmed_hits.iter().rev().position(|hit| {
            hit.direction != "incoming"
                && hit.target_max_hp > 0.0
                && hit.target_hp_after - current_hp >= MIN_FOLLOW_UP_RESIDUAL_DAMAGE
        })?;
        let source_index = self.recent_confirmed_hits.len() - 1 - source_index;
        let source = self.recent_confirmed_hits[source_index].clone();
        self.recent_confirmed_hits[source_index].target_hp_after = current_hp;
        self.recent_confirmed_hits[source_index].target_hp_percent = if source.target_max_hp > 0.0 {
            current_hp / source.target_max_hp * 100.0
        } else {
            0.0
        };
        let damage = source.target_hp_after - current_hp;
        let damage_attribute = source.damage_attribute.clone().or_else(|| {
            characters
                .get(&source.char_id)
                .and_then(|character| character.attribute.clone())
        });
        Some(HitFollowUp {
            source_timestamp: source.timestamp,
            source_char_id: source.char_id,
            source_damage: source.damage,
            source_target_hp_before: source.target_hp_before,
            source_target_hp_after: source.target_hp_after,
            source_target_max_hp: source.target_max_hp,
            source_gameplay_effect_index: source.gameplay_effect_index,
            timestamp,
            damage,
            target_hp_after: current_hp,
            target_hp_percent: if source.target_max_hp > 0.0 {
                current_hp / source.target_max_hp * 100.0
            } else {
                0.0
            },
            damage_name: Some("HP同步伤害".to_owned()),
            attack_type: Some("HP同步伤害".to_owned()),
            damage_attribute,
        })
    }

    fn suppress_matching_ambiguous_hits(&mut self, confirmed_hit: &Hit) -> usize {
        if !is_confirmed_packet_hit(confirmed_hit) {
            return 0;
        }
        let before = self.pending_ambiguous_hits.len();
        self.pending_ambiguous_hits
            .retain(|pending| !same_damage_event(pending, confirmed_hit));
        before - self.pending_ambiguous_hits.len()
    }
}

fn is_ambiguous_session_hit(hit: &Hit, declared_ids: &[u32]) -> bool {
    declared_ids.len() > 1
        && hit.char_source == "session"
        && hit.direction == "unknown"
        && hit.gameplay_effect_index.is_some()
}

fn gameplay_effect_confirms_session_hit(
    hit: &Hit,
    declared_ids: &[u32],
    characters: &HashMap<u32, CharacterInfo>,
) -> bool {
    if declared_ids.len() <= 1
        || hit.char_source != "session"
        || hit.direction != "unknown"
        || !declared_ids.contains(&hit.char_id)
    {
        return false;
    }
    let Some(effect_name) = hit.gameplay_effect_name.as_deref() else {
        return false;
    };
    let Some(character) = characters.get(&hit.char_id) else {
        return false;
    };
    let name = character.name_en.trim();
    !name.is_empty()
        && effect_name
            .to_ascii_lowercase()
            .contains(&name.to_ascii_lowercase())
}

fn is_confirmed_packet_hit(hit: &Hit) -> bool {
    matches!(hit.char_source.as_str(), "packet" | "gameplay_effect") && hit.direction == "outgoing"
}

fn same_damage_event(left: &Hit, right: &Hit) -> bool {
    (left.timestamp - right.timestamp).abs() <= AMBIGUOUS_HIT_CONFIRMATION_WINDOW_SECONDS
        && left.gameplay_effect_index.is_some()
        && left.gameplay_effect_index == right.gameplay_effect_index
        && nearly_same(left.damage, right.damage)
        && nearly_same(left.target_hp_before, right.target_hp_before)
        && nearly_same(left.target_hp_after, right.target_hp_after)
        && nearly_same(left.target_max_hp, right.target_max_hp)
}

fn is_recent_confirmed_duplicate(hit: &Hit, confirmed_hits: &[Hit]) -> bool {
    confirmed_hits.iter().any(|confirmed| {
        let timestamp_delta = (hit.timestamp - confirmed.timestamp).abs();
        hit.gameplay_effect_index.is_none()
            && confirmed.gameplay_effect_index.is_some()
            && timestamp_delta <= UNTYPED_SHADOW_HIT_WINDOW_SECONDS
            && hit.char_id == confirmed.char_id
            && nearly_same(hit.damage, confirmed.damage)
    })
}

fn nearly_same(left: f64, right: f64) -> bool {
    (left - right).abs() <= 0.5
}

fn fuwen_start_pair(
    payload: &[u8],
    evidence: &[(u32, u8, usize)],
    characters: &HashMap<u32, CharacterInfo>,
) -> Option<(u32, u32)> {
    if !matches_shifted_bytes_at(
        payload,
        FUWEN_START_SIGNATURE_SHIFT,
        FUWEN_START_SIGNATURE_OFFSET,
        FUWEN_START_SIGNATURE,
    ) {
        return None;
    }
    let entering_character_id = character_id_at_evidence_location(
        evidence,
        FUWEN_ENTERING_ID_SHIFT,
        FUWEN_ENTERING_ID_OFFSET,
    )?;
    let previous_character_id = character_id_at_evidence_location(
        evidence,
        FUWEN_PREVIOUS_ID_SHIFT,
        FUWEN_PREVIOUS_ID_OFFSET,
    )?;
    if entering_character_id == previous_character_id {
        return None;
    }
    let entering_attribute = characters
        .get(&entering_character_id)
        .and_then(|character| character.attribute.as_deref())?;
    let previous_attribute = characters
        .get(&previous_character_id)
        .and_then(|character| character.attribute.as_deref())?;
    let has_fuwen_pair = (entering_attribute == "灵" && previous_attribute == "咒")
        || (entering_attribute == "咒" && previous_attribute == "灵");
    has_fuwen_pair.then_some((entering_character_id, previous_character_id))
}

fn character_id_at_evidence_location(
    evidence: &[(u32, u8, usize)],
    bit_shift: u8,
    byte_offset: usize,
) -> Option<u32> {
    evidence
        .iter()
        .find(|(_, shift, offset)| *shift == bit_shift && *offset == byte_offset)
        .map(|(character_id, _, _)| *character_id)
}

fn character_debug_label(character_id: u32, characters: &HashMap<u32, CharacterInfo>) -> String {
    characters.get(&character_id).map_or_else(
        || character_id.to_string(),
        |character| {
            let name = if character.name_zh.is_empty() {
                character.name_en.as_str()
            } else {
                character.name_zh.as_str()
            };
            match character.attribute.as_deref() {
                Some(attribute) if !name.is_empty() => {
                    format!("{name}({character_id}/{attribute})")
                }
                Some(attribute) => format!("{character_id}/{attribute}"),
                None if !name.is_empty() => format!("{name}({character_id})"),
                None => character_id.to_string(),
            }
        },
    )
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
    let skill = skills.get(effect_name);
    if let Some(skill) = skill {
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
    if is_known_incoming_damage_effect(effect_name) {
        hit.direction = "incoming".to_owned();
    }
    if is_known_outgoing_damage_effect(effect_name, skill) {
        hit.direction = "outgoing".to_owned();
    }
    if is_vehicle_physical_damage_effect(effect_name) {
        hit.direction = "outgoing".to_owned();
        hit.damage_attribute = Some("物理".to_owned());
        hit.attack_type = Some("载具伤害".to_owned());
    }
}

fn character_id_from_ability_name(
    ability_name: &str,
    characters: &HashMap<u32, CharacterInfo>,
) -> Option<u32> {
    for token in ability_name.split('_') {
        let bytes = token.as_bytes();
        if bytes.len() < 4 {
            continue;
        }
        let suffix = &bytes[bytes.len() - 3..];
        if !suffix.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let prefix = &bytes[..bytes.len() - 3];
        if prefix.is_empty() || !prefix.iter().any(u8::is_ascii_alphabetic) {
            continue;
        }
        let id = 1000
            + suffix
                .iter()
                .fold(0_u32, |value, digit| value * 10 + (digit - b'0') as u32);
        if characters.contains_key(&id) {
            return Some(id);
        }
    }
    None
}

fn reattribute_hit_from_ability_name(
    hit: &mut Hit,
    can_override_packet_id: bool,
    characters: &HashMap<u32, CharacterInfo>,
) {
    if hit.direction == "incoming" {
        return;
    }
    if hit.char_source == "packet" && !can_override_packet_id {
        return;
    }
    let Some(ability_name) = hit.ability_name.as_deref() else {
        return;
    };
    let Some(character_id) = character_id_from_ability_name(ability_name, characters) else {
        return;
    };
    if character_id == hit.char_id {
        return;
    }
    set_hit_character(hit, character_id, characters);
    hit.char_source = "gameplay_effect".to_owned();
}

fn is_known_outgoing_damage_effect(effect_name: &str, skill: Option<&GameplayEffectSkill>) -> bool {
    let effect_name_lower = effect_name.to_ascii_lowercase();
    if effect_name_lower.starts_with("ge_mon_") {
        return false;
    }
    if effect_name.starts_with("GE_Player_") && effect_name.contains("_Damage") {
        return true;
    }
    if effect_name.starts_with("GE_ActorReaction_")
        || effect_name.starts_with("GE_Reaction")
        || effect_name.starts_with("Buff_Reaction_")
    {
        return true;
    }
    if effect_name_lower.contains("tenacity") && effect_name_lower.contains("damage") {
        return true;
    }
    let damage_like_effect = effect_name_lower.contains("damage")
        || effect_name_lower.contains("_dmg")
        || effect_name_lower.ends_with("_dmg");
    damage_like_effect
        && skill.is_some_and(|skill| {
            skill
                .ability_name
                .as_deref()
                .is_some_and(|ability_name| ability_name.starts_with("GA_"))
        })
}

fn is_known_incoming_damage_effect(effect_name: &str) -> bool {
    let effect_name_lower = effect_name.to_ascii_lowercase();
    effect_name_lower.starts_with("ge_mon_")
        && !effect_name_lower.contains("steal")
        && (effect_name_lower.contains("damage") || effect_name_lower.contains("_dmg"))
}

fn is_vehicle_physical_damage_effect(effect_name: &str) -> bool {
    effect_name.starts_with("GE_Vehicle_HitOut")
        || effect_name.starts_with("GE_VehicleCombatDamage")
        || effect_name == "GE_Player_VehicleExplode_HitOut"
}

fn resolve_damage_name(
    effect_name: &str,
    skills: &HashMap<String, GameplayEffectSkill>,
    wooden_names: &HashMap<String, String>,
) -> Option<String> {
    if let Some(name) = wooden_names.get(effect_name) {
        return Some(name.clone());
    }
    if let Some(base_effect_name) = effect_name
        .strip_suffix("_Explode_Damage")
        .map(|prefix| format!("{prefix}_Damage"))
        && let Some(name) = wooden_names.get(&base_effect_name)
    {
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

fn set_hit_character(hit: &mut Hit, new_char_id: u32, characters: &HashMap<u32, CharacterInfo>) {
    let character = characters.get(&new_char_id);
    hit.char_id = new_char_id;
    hit.char_known = character.is_some();
    hit.char_name = character
        .map(|row| {
            if row.name_zh.is_empty() {
                row.name_en.clone()
            } else {
                row.name_zh.clone()
            }
        })
        .unwrap_or_else(|| format!("未知角色({new_char_id})"));
}

/// The two character attributes whose 环合 produces a given reaction *burst*
/// (`Buff_Reaction_*`, classified by [`classify_attack_type`]). Only reactions
/// that are attribute-locked and whose damage packets carry no caster need this;
/// returns `None` for everything else. Mirrors the pairings in `qte_reaction_type`.
fn reaction_owner_attributes(attack_type: &str) -> Option<[&'static str; 2]> {
    match attack_type {
        // 黯星 = 暗 + 魂. See `qte_reaction_type("暗", "魂")`.
        "黯星" => Some(["暗", "魂"]),
        _ => None,
    }
}

/// Re-home a reaction burst that was credited to a character who can't produce it.
///
/// Reactions like `黯星` carry no caster in their damage record, so
/// [`parse_damage_payload`] attributes them to whatever single character the
/// packet happened to declare. When the game bundles such a tick into an
/// unrelated character's replication packet (e.g. a 咒 character who is merely
/// on-field), the credit lands on someone whose attribute can't generate the
/// reaction. Detect that and move it to the most recently declared character
/// whose attribute *can* — i.e. the on-field reaction participant.
///
/// No-op when the reaction isn't attribute-locked, when the current owner is
/// already plausible, or when no recent valid owner is on record.
fn reattribute_orphan_reaction(
    hit: &mut Hit,
    character_declarations: &HashMap<u32, f64>,
    timestamp: f64,
    characters: &HashMap<u32, CharacterInfo>,
) {
    let Some(valid_attributes) = hit
        .attack_type
        .as_deref()
        .and_then(reaction_owner_attributes)
    else {
        return;
    };
    let attribute_of = |character_id: &u32| {
        characters
            .get(character_id)
            .and_then(|character| character.attribute.as_deref())
    };
    if attribute_of(&hit.char_id).is_some_and(|attribute| valid_attributes.contains(&attribute)) {
        return; // already credited to a character that can produce this reaction
    }
    let Some(new_char_id) = character_declarations
        .iter()
        .filter(|(character_id, declared_at)| {
            timestamp - **declared_at <= REACTION_REATTRIBUTION_WINDOW_SECONDS
                && attribute_of(character_id)
                    .is_some_and(|attribute| valid_attributes.contains(&attribute))
        })
        .max_by(|left, right| left.1.total_cmp(right.1))
        .map(|(character_id, _)| *character_id)
    else {
        return; // no on-field 暗/魂 character to credit — leave attribution as-is
    };
    if new_char_id == hit.char_id {
        return;
    }
    set_hit_character(hit, new_char_id, characters);
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
        let expired_hits = self.take_expired_ambiguous_hits(timestamp);
        self.emit_hits(expired_hits, characters, sender);

        let decoded_text = decode_payload_text(payload);
        let evidence = find_declared_character_evidence(payload);
        let final_tower_evidence = find_final_tower_character_evidence(payload);
        let character_evidence = merged_character_evidence(&evidence, &final_tower_evidence);
        let ids = character_ids_from_evidence_sources(&evidence, &final_tower_evidence);
        self.follow_up_damage
            .observe_characters(ids.iter().copied(), characters);
        let outgoing = infer_outgoing(src, src_port, dst, local_ip, &ids, &self.client_endpoints);
        if outgoing && !ids.is_empty() {
            self.client_endpoints.insert((src, src_port));
        }
        let direction = if outgoing { "C2S" } else { "S2C" };
        let gameplay_effects = parse_gameplay_effects(payload);
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
        for hit in &mut hits {
            enrich_hit_with_gameplay_effect(
                hit,
                &gameplay_effects,
                &self.gameplay_effect_names,
                &self.gameplay_effect_skills,
                &self.wooden_damage_names,
            );
            reattribute_hit_from_ability_name(hit, !final_tower_evidence.is_empty(), characters);
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
                    && let Some(reaction_type) =
                        qte_reaction_type(previous_attribute, entering_attribute)
                {
                    hit.attack_type = Some(format!("环合·{reaction_type}"));
                }
            }
            reattribute_orphan_reaction(hit, &self.character_declarations, timestamp, characters);
            self.follow_up_damage.observe_fuwen_trigger_hit(hit);
        }
        let mut time_stop_events = self.ultra_time_stop.events_from_packet(
            timestamp,
            &decoded_text,
            &ids,
            &self.ultra_time_stops,
        );
        time_stop_events.extend(
            self.ultra_time_stop
                .events_from_hits(&hits, &self.ultra_time_stops),
        );
        let prepared_hits =
            self.prepare_hits_for_emission(hits, &ids, include_incoming, characters);
        for character_id in &ids {
            self.character_declarations.insert(*character_id, timestamp);
        }
        self.character_declarations
            .retain(|_, declared_at| timestamp - *declared_at <= 10.0);
        let accepted = prepared_hits.emit.len();
        let preview_len = payload.len().min(96);
        let payload_hex = hex::encode(payload);
        // CurrentHP 候选缺少目标 handle 校验，仅用于调试显示，不参与 follow-up 计算。
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
        let fuwen_start = if !outgoing
            && gameplay_effects.is_empty()
            && current_hp_updates.is_empty()
            && boss_hp_updates.is_empty()
        {
            fuwen_start_pair(payload, &evidence, characters)
        } else {
            None
        };
        if let Some((entering_character_id, previous_character_id)) = fuwen_start {
            self.follow_up_damage.observe_fuwen_start_candidate(
                timestamp,
                entering_character_id,
                previous_character_id,
                characters,
            );
        }
        for event in time_stop_events {
            let _ = sender.send(EngineEvent::TimeStop(event));
        }
        if current_hp_updates.is_empty()
            && boss_hp_updates.is_empty()
            && prepared_hits.deferred_ambiguous == 0
            && fuwen_start.is_none()
            && !should_keep_debug_packet(payload, &ids, accepted, &decoded_text)
        {
            return;
        }
        let mut note = String::new();
        if prepared_hits.filtered_incoming > 0 {
            append_packet_note(
                &mut note,
                Some(format!(
                    "过滤 {} 条 incoming 记录",
                    prepared_hits.filtered_incoming
                )),
            );
        }
        if prepared_hits.deferred_ambiguous > 0 {
            append_packet_note(
                &mut note,
                Some(format!(
                    "暂存 {} 条多角色候选伤害等待确认",
                    prepared_hits.deferred_ambiguous
                )),
            );
        }
        if prepared_hits.suppressed_ambiguous > 0 {
            append_packet_note(
                &mut note,
                Some(format!(
                    "丢弃 {} 条已确认重复候选伤害",
                    prepared_hits.suppressed_ambiguous
                )),
            );
        }
        if let Some((entering_character_id, previous_character_id)) = fuwen_start {
            append_packet_note(
                &mut note,
                Some(format!(
                    "覆纹启动：{} + {}",
                    character_debug_label(entering_character_id, characters),
                    character_debug_label(previous_character_id, characters)
                )),
            );
        }
        append_packet_note(
            &mut note,
            binary_payload_diagnostic(payload, direction, &decoded_text, &character_evidence),
        );
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
                            "{:.0}@{}:{}",
                            update.current_hp, update.byte_offset, update.bit_shift
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
        let inferred_follow_ups = boss_hp_updates
            .iter()
            .filter_map(|update| {
                self.follow_up_damage
                    .observe_server_hp(timestamp, update.current_hp as f64)
            })
            .collect::<Vec<_>>();
        let hp_sync_follow_ups = if self.use_server_damage_calibration {
            Vec::new()
        } else {
            boss_hp_updates
                .iter()
                .filter_map(|update| {
                    self.infer_boss_hp_sync_damage(timestamp, update.current_hp as f64, characters)
                })
                .collect::<Vec<_>>()
        };
        let server_damage_corrections = if self.use_server_damage_calibration {
            boss_hp_updates
                .iter()
                .filter_map(|update| {
                    self.server_damage_calibration
                        .observe_boss_hp(timestamp, update)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if let Some(TransportPacket::Sequenced(packet)) = parse_transport_packet(payload) {
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
            } else if let Some(bunch) = parse_single_bunch(&packet) {
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
        self.emit_hits(prepared_hits.emit, characters, sender);
        for follow_up in inferred_follow_ups {
            let _ = sender.send(EngineEvent::HitFollowUp(follow_up));
        }
        for follow_up in hp_sync_follow_ups {
            let _ = sender.send(EngineEvent::HitFollowUp(follow_up));
        }
        for correction in server_damage_corrections {
            let _ = sender.send(EngineEvent::HitDamageCorrection(correction));
        }
    }
}

pub fn start_capture(
    device: CaptureDevice,
    local_ip: Option<Ipv4Addr>,
    filter: String,
    include_incoming: bool,
    use_server_damage_calibration: bool,
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
            use_server_damage_calibration,
            characters,
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
    use_server_damage_calibration: bool,
    characters: Arc<HashMap<u32, CharacterInfo>>,
    sender: &'a Sender<EngineEvent>,
    stop: &'a AtomicBool,
    raw_capture: &'a RawCaptureBuffer,
}

/// Parser thread body: drains decoded frames off the bounded queue and runs the stable decode
/// pipeline, fully decoupled from packet acquisition. It owns its own `PacketDecoder` and exits
/// once the acquisition thread drops the frame sender, flushing any deferred ambiguous hits.
fn run_parser(
    frames: Receiver<CaptureFrame>,
    local_ip: Option<Ipv4Addr>,
    include_incoming: bool,
    use_server_damage_calibration: bool,
    characters: Arc<HashMap<u32, CharacterInfo>>,
    sender: Sender<EngineEvent>,
) {
    let mut decoder = PacketDecoder::with_server_damage_calibration(use_server_damage_calibration);
    if let Some(warning) = decoder.resource_warning() {
        let _ = sender.send(EngineEvent::Warning(warning));
    }
    while let Ok(frame) = frames.recv() {
        decoder.process_ethernet_frame(
            &frame.data,
            frame.timestamp,
            local_ip,
            include_incoming,
            &characters,
            &sender,
        );
    }
    let pending_hits = decoder.take_all_ambiguous_hits();
    decoder.emit_hits(pending_hits, &characters, &sender);
}

fn run_capture(config: CaptureRunConfig<'_>) -> Result<(), String> {
    let CaptureRunConfig {
        device,
        local_ip,
        filter,
        include_incoming,
        use_server_damage_calibration,
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

        // Decode on a dedicated thread so a parse latency spike cannot stall the acquisition loop
        // below. The acquisition thread only reads frames, writes the raw PCAPNG, and forwards a
        // copy onto the bounded queue.
        let (frame_sender, frame_receiver) = bounded::<CaptureFrame>(CAPTURE_FRAME_QUEUE_CAPACITY);
        let parser_thread = {
            let characters = Arc::clone(&characters);
            let sender = sender.clone();
            thread::spawn(move || {
                run_parser(
                    frame_receiver,
                    local_ip,
                    include_incoming,
                    use_server_damage_calibration,
                    characters,
                    sender,
                );
            })
        };

        let mut loop_result = Ok(());
        while !stop.load(Ordering::Relaxed) {
            let mut header = ptr::null();
            let mut packet_data = ptr::null();
            let result = next_ex(handle.as_ptr(), &mut header, &mut packet_data);
            if result == 0 {
                continue;
            }
            if result < 0 {
                let error = c_string(get_err(handle.as_ptr()));
                loop_result = Err(format!("failed to read packet: {error}"));
                break;
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
            match frame_sender.try_send(CaptureFrame {
                data: packet.to_vec(),
                timestamp,
            }) {
                // Queue full: drop for live parsing only. The raw frame is already persisted above
                // and remains recoverable via Debug replay, so acquisition keeps draining Npcap.
                Ok(()) | Err(TrySendError::Full(_)) => {}
                // Parser thread ended unexpectedly; nothing left to feed.
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        drop(frame_sender);
        let _ = parser_thread.join();
        loop_result?;
    }
    Ok(())
}

pub fn import_pcapng(
    path: PathBuf,
    characters: Arc<HashMap<u32, CharacterInfo>>,
    local_ip_hint: Option<Ipv4Addr>,
    include_incoming: bool,
    use_server_damage_calibration: bool,
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
            let mut decoder =
                PacketDecoder::with_server_damage_calibration(use_server_damage_calibration);
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
                decoder.process_ethernet_frame(
                    &data,
                    timestamp,
                    local_ip_hint,
                    include_incoming,
                    &characters,
                    &sender,
                );
            }
            let pending_hits = decoder.take_all_ambiguous_hits();
            decoder.emit_hits(pending_hits, &characters, &sender);
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
    #[serde(default)]
    damage_attribute: Option<String>,
    #[serde(default)]
    follow_up_damage: f64,
    #[serde(default)]
    follow_up_timestamp: Option<f64>,
    #[serde(default)]
    follow_up_damage_name: Option<String>,
    #[serde(default)]
    follow_up_attack_type: Option<String>,
    #[serde(default)]
    follow_up_damage_attribute: Option<String>,
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
            let ultra_time_stops = find_data_file(Path::new(ULTRA_TIME_STOP_DATA_PATH))
                .and_then(|path| load_ultra_time_stops(&path).ok())
                .unwrap_or_default();
            let mut ultra_time_stop = UltraTimeStopTracker::default();
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
                    if send_export_packet(packet, &sender, &ultra_time_stops, &mut ultra_time_stop)?
                    {
                        packet_count += 1;
                    }
                } else {
                    let hit = hits.next().expect("peeked hit must exist");
                    let event = export_hit_event(hit);
                    if let EngineEvent::Hit(hit) = &event {
                        for time_stop in ultra_time_stop
                            .events_from_hits(std::slice::from_ref(hit.as_ref()), &ultra_time_stops)
                        {
                            sender
                                .send(EngineEvent::TimeStop(time_stop))
                                .map_err(|error| error.to_string())?;
                        }
                    }
                    sender.send(event).map_err(|error| error.to_string())?;
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
    sender: &Sender<EngineEvent>,
    ultra_time_stops: &HashMap<u32, UltraTimeStopEntry>,
    ultra_time_stop: &mut UltraTimeStopTracker,
) -> Result<bool, String> {
    let mut declared_ids = parse_export_ids(&packet.declared_ids);
    let payload = if packet.payload_hex.trim().is_empty() {
        Vec::new()
    } else {
        hex::decode(&packet.payload_hex).map_err(|error| format!("payload_hex 无效: {error}"))?
    };
    let evidence = find_declared_character_evidence(&payload);
    let final_tower_evidence = find_final_tower_character_evidence(&payload);
    let character_evidence = merged_character_evidence(&evidence, &final_tower_evidence);
    append_unique_ids(
        &mut declared_ids,
        character_ids_from_evidence_sources(&evidence, &final_tower_evidence),
    );
    let decoded_text = if packet.decoded_text.trim().is_empty() && !payload.is_empty() {
        decode_payload_text(&payload)
    } else {
        packet.decoded_text
    };
    for event in ultra_time_stop.events_from_packet(
        packet.timestamp_unix,
        &decoded_text,
        &declared_ids,
        ultra_time_stops,
    ) {
        sender
            .send(EngineEvent::TimeStop(event))
            .map_err(|error| error.to_string())?;
    }
    if !should_keep_debug_packet(&payload, &declared_ids, packet.parsed_hits, &decoded_text) {
        return Ok(false);
    }
    let mut note = packet.note;
    append_packet_note(
        &mut note,
        binary_payload_diagnostic(
            &payload,
            &packet.direction,
            &decoded_text,
            &character_evidence,
        ),
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
        .send(EngineEvent::Packet(Box::new(packet)))
        .map_err(|error| error.to_string())?;
    Ok(true)
}

fn export_hit_event(hit: ExportHit) -> EngineEvent {
    EngineEvent::Hit(Box::new(Hit {
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
        damage_attribute: hit.damage_attribute,
        follow_up_damage: hit.follow_up_damage,
        follow_up_timestamp: hit.follow_up_timestamp,
        follow_up_damage_name: hit.follow_up_damage_name,
        follow_up_attack_type: hit.follow_up_attack_type,
        follow_up_damage_attribute: hit.follow_up_damage_attribute,
    }))
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
    use crossbeam_channel::unbounded;

    use crate::parser::{CHARACTER_DATA_PATH, UltraTimeStopCooldown, load_characters};

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
        tracker.observe_fuwen_start_candidate(0.0, 1, 2, &characters);
        observe_visible_fuwen_trigger(&mut tracker, 1, 0.0);
        let mut hit = targetless_hit();
        hit.char_id = 1;
        hit.char_name = "character1".to_owned();
        hit.target_max_hp = 1_000_000.0;
        hit.target_hp_before = 1_000_000.0;
        hit.damage = 3_177.0;
        for index in 0..MAX_PENDING_FOLLOW_UP_HITS + 20 {
            hit.timestamp = index as f64 / 1_000.0;
            tracker.observe_hit(&hit, Some(241), &characters);
        }
        assert_eq!(tracker.pending_hits.len(), MAX_PENDING_FOLLOW_UP_HITS);

        hit.timestamp = 2.0;
        tracker.observe_hit(&hit, Some(241), &characters);
        assert_eq!(tracker.pending_hits.len(), 1);
        let follow_up = tracker
            .observe_server_hp(2.1, 996_150.0)
            .expect("recent pending hit should still resolve follow-up damage");
        assert_eq!(follow_up.damage, 673.0);
        assert_eq!(follow_up.source_char_id, 1);
        assert_eq!(follow_up.source_damage, 3_177.0);
        assert_eq!(follow_up.damage_name.as_deref(), Some("覆纹追加攻击"));
        assert_eq!(follow_up.attack_type.as_deref(), Some("覆纹"));
        assert_eq!(follow_up.damage_attribute.as_deref(), Some("灵"));
    }

    #[test]
    fn follow_up_requires_visible_fuwen_trigger() {
        let characters = follow_up_test_characters();
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_characters([1, 2], &characters);
        let mut hit = targetless_hit();
        hit.char_id = 1;
        hit.target_max_hp = 1_000_000.0;
        hit.target_hp_before = 1_000_000.0;
        hit.damage = 1_000.0;

        tracker.observe_hit(&hit, None, &characters);

        assert!(tracker.observe_server_hp(0.1, 998_750.0).is_none());
    }

    #[test]
    fn visible_fuwen_trigger_without_start_packet_records_follow_up() {
        let characters = follow_up_test_characters();
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_characters([1, 2], &characters);
        observe_visible_fuwen_trigger(&mut tracker, 1, 0.0);
        let mut hit = targetless_hit();
        hit.char_id = 2;
        hit.target_max_hp = 1_000_000.0;
        hit.target_hp_before = 800_000.0;
        hit.damage = 1_000.0;

        tracker.observe_hit(&hit, None, &characters);
        let follow_up = tracker
            .observe_server_hp(0.1, 798_750.0)
            .expect("visible fuwen trigger should be enough to open follow-up tracking");
        assert_eq!(follow_up.damage, 250.0);
        assert_eq!(follow_up.damage_attribute.as_deref(), Some("咒"));
    }

    fn character_with_attribute(name_zh: &str, attribute: &str) -> CharacterInfo {
        CharacterInfo {
            name_zh: name_zh.to_owned(),
            name_en: String::new(),
            color: None,
            avatar: None,
            attribute: Some(attribute.to_owned()),
        }
    }

    #[test]
    fn orphan_dark_star_reaction_is_rehomed_to_dark_or_soul_owner() {
        let characters = HashMap::from([
            (1003, character_with_attribute("早雾", "咒")),
            (1004, character_with_attribute("安魂曲", "暗")),
            (1020, character_with_attribute("哈尼娅", "魂")),
        ]);
        // 暗 (安魂曲) is the most recently declared reaction participant.
        let declarations = HashMap::from([(1004_u32, 10.0_f64), (1020_u32, 8.0_f64)]);

        // 黯星 burst mis-credited to 早雾 (咒) because the packet declared 早雾.
        let mut orphan = targetless_hit();
        orphan.char_id = 1003;
        orphan.char_name = "早雾".to_owned();
        orphan.attack_type = Some("黯星".to_owned());
        reattribute_orphan_reaction(&mut orphan, &declarations, 10.001, &characters);
        assert_eq!(orphan.char_id, 1004);
        assert_eq!(orphan.char_name, "安魂曲");
        assert!(orphan.char_known);

        // Already on a 暗 character: untouched.
        let mut already_ok = targetless_hit();
        already_ok.char_id = 1004;
        already_ok.attack_type = Some("黯星".to_owned());
        reattribute_orphan_reaction(&mut already_ok, &declarations, 10.001, &characters);
        assert_eq!(already_ok.char_id, 1004);

        // No recent 暗/魂 declaration in window: left as-is rather than guessed.
        let mut stale = targetless_hit();
        stale.char_id = 1003;
        stale.attack_type = Some("黯星".to_owned());
        reattribute_orphan_reaction(&mut stale, &declarations, 99.0, &characters);
        assert_eq!(stale.char_id, 1003);

        // A non-attribute-locked reaction is never rehomed.
        let mut other = targetless_hit();
        other.char_id = 1003;
        other.attack_type = Some("普攻".to_owned());
        reattribute_orphan_reaction(&mut other, &declarations, 10.001, &characters);
        assert_eq!(other.char_id, 1003);
    }

    #[test]
    fn creation_flower_does_not_trigger_fuwen_follow_up() {
        let characters = follow_up_test_characters();
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_characters([1, 2], &characters);
        observe_visible_fuwen_trigger(&mut tracker, 1, 0.0);
        let mut hit = targetless_hit();
        hit.char_id = 2;
        hit.target_max_hp = 1_000_000.0;
        hit.target_hp_before = 800_000.0;
        hit.damage = 1_000.0;
        hit.attack_type = Some("创生花".to_owned());

        tracker.observe_hit(&hit, None, &characters);

        assert!(tracker.observe_server_hp(0.1, 798_750.0).is_none());
        assert!(tracker.fuwen_active);
    }

    #[test]
    fn non_ling_zhou_hit_does_not_end_active_fuwen() {
        let characters = follow_up_test_characters();
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_fuwen_start_candidate(0.0, 1, 2, &characters);
        observe_visible_fuwen_trigger(&mut tracker, 1, 0.0);
        let mut hit = targetless_hit();
        hit.target_max_hp = 1_000_000.0;

        hit.char_id = 3;
        hit.target_hp_before = 1_000_000.0;
        hit.damage = 500.0;
        tracker.observe_hit(&hit, None, &characters);
        assert!(tracker.observe_server_hp(0.1, 999_500.0).is_none());
        assert!(tracker.fuwen_active);

        hit.char_id = 1;
        hit.timestamp = 0.2;
        hit.target_hp_before = 999_500.0;
        hit.damage = 1_000.0;
        tracker.observe_hit(&hit, None, &characters);
        let follow_up = tracker
            .observe_server_hp(0.3, 998_250.0)
            .expect("ling/zhou residual should still be recorded after other-attribute hit");
        assert_eq!(follow_up.damage, 250.0);
    }

    #[test]
    fn zero_residual_does_not_end_active_fuwen_after_recorded_residual() {
        let characters = follow_up_test_characters();
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_fuwen_start_candidate(0.0, 1, 2, &characters);
        observe_visible_fuwen_trigger(&mut tracker, 1, 0.0);
        let mut hit = targetless_hit();
        hit.char_id = 1;
        hit.target_max_hp = 1_000_000.0;
        hit.target_hp_before = 1_000_000.0;
        hit.damage = 1_000.0;

        tracker.observe_hit(&hit, None, &characters);

        assert!(tracker.observe_server_hp(0.1, 999_000.0).is_none());
        assert!(tracker.fuwen_active);

        hit.timestamp = 0.2;
        hit.target_hp_before = 999_000.0;
        hit.damage = 1_000.0;
        hit.attack_type = Some("普攻".to_owned());
        tracker.observe_hit(&hit, None, &characters);
        let follow_up = tracker
            .observe_server_hp(0.3, 997_750.0)
            .expect("first residual after fuwen start should be recorded");
        assert_eq!(follow_up.damage, 250.0);
        assert!(tracker.fuwen_active);

        hit.timestamp = 0.4;
        hit.target_hp_before = 997_750.0;
        hit.damage = 1_000.0;
        tracker.observe_hit(&hit, None, &characters);
        assert!(tracker.observe_server_hp(0.5, 996_750.0).is_none());
        assert!(tracker.fuwen_active);
    }

    #[test]
    fn fuwen_stays_active_across_long_gaps_until_battle_reset() {
        let characters = follow_up_test_characters();
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_fuwen_start_candidate(0.0, 1, 2, &characters);
        observe_visible_fuwen_trigger(&mut tracker, 1, 0.0);
        let mut hit = targetless_hit();
        hit.char_id = 1;
        hit.target_max_hp = 1_000_000.0;
        hit.target_hp_before = 800_000.0;
        hit.damage = 1_000.0;

        hit.timestamp = 60.0;
        tracker.observe_hit(&hit, None, &characters);
        let follow_up = tracker
            .observe_server_hp(60.1, 798_750.0)
            .expect("fuwen follow-up should not expire just because of a long idle gap");
        assert_eq!(follow_up.damage, 250.0);
        assert!(tracker.fuwen_active);
    }

    #[test]
    fn hidden_fuwen_candidate_does_not_record_without_visible_trigger() {
        let characters = follow_up_test_characters();
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_fuwen_start_candidate(0.0, 1, 2, &characters);
        let mut hit = targetless_hit();
        hit.timestamp = 5.0;
        hit.char_id = 2;
        hit.target_max_hp = 1_000_000.0;
        hit.target_hp_before = 1_000_000.0;
        hit.damage = 1_000.0;
        hit.attack_type = Some("普攻".to_owned());

        tracker.observe_hit(&hit, None, &characters);
        assert!(tracker.observe_server_hp(5.1, 999_000.0).is_none());
        assert!(!tracker.fuwen_active);
        assert!(tracker.fuwen_start_pending);

        hit.char_id = 1;
        hit.timestamp = 5.2;
        hit.target_hp_before = 999_000.0;
        hit.attack_type = Some("Q技能".to_owned());
        tracker.observe_hit(&hit, None, &characters);
        assert!(tracker.observe_server_hp(5.3, 998_000.0).is_none());
        assert!(!tracker.fuwen_active);
        assert!(tracker.fuwen_start_pending);
    }

    #[test]
    fn visible_fuwen_trigger_activates_follow_up_window() {
        let characters = follow_up_test_characters();
        let mut tracker = FollowUpDamageTracker::default();
        tracker.observe_characters([1, 2], &characters);
        observe_visible_fuwen_trigger(&mut tracker, 1, 5.0);

        assert!(tracker.fuwen_active);
        assert!(!tracker.fuwen_start_pending);
    }

    #[test]
    fn fuwen_start_pair_uses_shifted_signature_and_fixed_role_positions() {
        let mut payload = vec![0_u8; 90];
        write_shifted_bytes(
            &mut payload,
            FUWEN_START_SIGNATURE_SHIFT,
            FUWEN_START_SIGNATURE_OFFSET,
            FUWEN_START_SIGNATURE,
        );
        write_shifted_bytes(
            &mut payload,
            FUWEN_ENTERING_ID_SHIFT,
            FUWEN_ENTERING_ID_OFFSET,
            &character_evidence_row(1001),
        );
        write_shifted_bytes(
            &mut payload,
            FUWEN_PREVIOUS_ID_SHIFT,
            FUWEN_PREVIOUS_ID_OFFSET,
            &character_evidence_row(1002),
        );
        let evidence = find_declared_character_evidence(&payload);
        let characters = HashMap::from([
            (
                1001,
                CharacterInfo {
                    name_zh: "entering".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: None,
                    attribute: Some("灵".to_owned()),
                },
            ),
            (
                1002,
                CharacterInfo {
                    name_zh: "previous".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: None,
                    attribute: Some("咒".to_owned()),
                },
            ),
        ]);

        assert_eq!(
            fuwen_start_pair(&payload, &evidence, &characters),
            Some((1001, 1002))
        );
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
    fn final_tower_ids_merge_with_declared_ids() {
        let declared = [(1001, 0, 4)];
        let final_tower = [(1076, 5, 30), (1001, 5, 52)];

        assert_eq!(
            character_ids_from_evidence_sources(&declared, &final_tower),
            [1001, 1076]
        );
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
            send_export_packet(
                packet,
                &sender,
                &HashMap::new(),
                &mut UltraTimeStopTracker::default()
            )
            .unwrap_err()
            .contains("payload_hex 无效")
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn send_export_packet_adds_final_tower_ids_from_payload() {
        let (sender, receiver) = unbounded();
        let payload = b"FCharacterForNet....ft_character_1076".to_vec();
        let packet = ExportPacket {
            timestamp_unix: 1.0,
            source: "127.0.0.1:1234".to_owned(),
            destination: "127.0.0.1:5678".to_owned(),
            direction: "C2S".to_owned(),
            payload_len: payload.len(),
            declared_ids: serde_json::json!([]),
            parsed_hits: 0,
            note: String::new(),
            payload_preview: hex::encode(&payload[..payload.len().min(48)]),
            payload_hex: hex::encode(&payload),
            decoded_text: String::new(),
        };

        assert!(
            send_export_packet(
                packet,
                &sender,
                &HashMap::new(),
                &mut UltraTimeStopTracker::default()
            )
            .is_ok()
        );
        match receiver.try_recv().expect("packet event should be emitted") {
            EngineEvent::Packet(packet) => {
                assert_eq!(packet.declared_ids, [1076]);
                assert!(packet.decoded_text.contains("ft_character_1076"));
            }
            _ => panic!("expected packet event"),
        }
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
        assert!(
            send_export_packet(
                packet,
                &sender,
                &HashMap::new(),
                &mut UltraTimeStopTracker::default()
            )
            .is_ok()
        );
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
    fn send_export_packet_preserves_exported_decoded_text() {
        let (sender, receiver) = unbounded();
        let packet = ExportPacket {
            timestamp_unix: 1.0,
            source: "127.0.0.1:1234".to_owned(),
            destination: "127.0.0.1:5678".to_owned(),
            direction: "S2C".to_owned(),
            payload_len: 2,
            declared_ids: serde_json::json!([]),
            parsed_hits: 1,
            note: String::new(),
            payload_preview: "0000".to_owned(),
            payload_hex: "0000".to_owned(),
            decoded_text: "导出时的协议文本".to_owned(),
        };

        assert!(
            send_export_packet(
                packet,
                &sender,
                &HashMap::new(),
                &mut UltraTimeStopTracker::default()
            )
            .is_ok()
        );
        match receiver.try_recv().expect("packet event should be emitted") {
            EngineEvent::Packet(packet) => {
                assert_eq!(packet.decoded_text, "导出时的协议文本");
                assert_eq!(packet.payload_hex, "0000");
            }
            _ => panic!("expected packet event"),
        }
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn send_export_packet_emits_pending_time_stop_end_before_filtering() {
        let (sender, receiver) = unbounded();
        let table = HashMap::from([(
            1010,
            UltraTimeStopEntry {
                ability_id: "GA_Nanally_UltraSkill".to_owned(),
                end_ability_event_seconds: 3.584608,
                source: "test".to_owned(),
                confidence: "high".to_owned(),
                ..UltraTimeStopEntry::default()
            },
        )]);
        let mut tracker = UltraTimeStopTracker::default();
        let start_packet = ExportPacket {
            timestamp_unix: 10.0,
            source: "127.0.0.1:1234".to_owned(),
            destination: "127.0.0.1:5678".to_owned(),
            direction: "C2S".to_owned(),
            payload_len: 0,
            declared_ids: serde_json::json!([]),
            parsed_hits: 0,
            note: String::new(),
            payload_preview: String::new(),
            payload_hex: String::new(),
            decoded_text: "CoolDown.Player.UltraSkill.F".to_owned(),
        };
        assert!(send_export_packet(start_packet, &sender, &table, &mut tracker).unwrap());
        assert!(matches!(
            receiver
                .try_recv()
                .expect("pending start should be emitted"),
            EngineEvent::TimeStop(TimeStopEvent::ExtraStart {
                timestamp: 10.0,
                ..
            })
        ));
        assert!(matches!(
            receiver.try_recv().expect("debug packet should be emitted"),
            EngineEvent::Packet(_)
        ));

        let ignored_packet = ExportPacket {
            timestamp_unix: 15.0,
            source: "127.0.0.1:1234".to_owned(),
            destination: "127.0.0.1:5678".to_owned(),
            direction: "C2S".to_owned(),
            payload_len: 4,
            declared_ids: serde_json::json!([]),
            parsed_hits: 0,
            note: String::new(),
            payload_preview: "00000000".to_owned(),
            payload_hex: "00000000".to_owned(),
            decoded_text: String::new(),
        };
        assert!(!send_export_packet(ignored_packet, &sender, &table, &mut tracker).unwrap());
        assert!(matches!(
            receiver.try_recv().expect("pending end should be emitted"),
            EngineEvent::TimeStop(TimeStopEvent::ExtraEnd {
                timestamp: 10.0,
                ..
            })
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn ultra_cooldown_packet_emits_fixed_time_stop_once() {
        let table = HashMap::from([(
            1052,
            UltraTimeStopEntry {
                ability_id: "GA_Jin_UltraSkill".to_owned(),
                end_ability_event_seconds: 2.183333,
                source: "test".to_owned(),
                confidence: "high".to_owned(),
                ..UltraTimeStopEntry::default()
            },
        )]);
        let mut tracker = UltraTimeStopTracker::default();
        let events =
            tracker.events_from_packet(10.0, "CoolDown.Player.UltraSkill.F", &[1052], &table);

        assert_eq!(
            events,
            vec![TimeStopEvent::UltraAnimation {
                timestamp: 10.0,
                char_id: 1052,
                ability_id: "GA_Jin_UltraSkill".to_owned(),
                duration_seconds: 2.183333,
            }]
        );
        assert!(
            tracker
                .events_from_packet(11.0, "CoolDown.Player.UltraSkill.F", &[1052], &table)
                .is_empty()
        );
    }

    #[test]
    fn shinku_special_cooldowns_emit_two_distinct_time_stop_segments() {
        let table = HashMap::from([(
            1076,
            UltraTimeStopEntry {
                ability_id: "GA_Shinku_UltraSkill".to_owned(),
                end_ability_event_seconds: 7.300015,
                extra_cooldowns: vec![UltraTimeStopCooldown {
                    cooldown_tag: "CoolDown.Player.UltraSkill.Shinku.UltraRage".to_owned(),
                    ability_id: "GA_Shinku_UltraSkill_Rage".to_owned(),
                    duration_seconds: 5.300056,
                }],
                ignored_cooldown_tags: vec![
                    "CoolDown.Player.UltraSkill.Shinku.UltraPre".to_owned(),
                ],
                source: "test".to_owned(),
                confidence: "high".to_owned(),
            },
        )]);
        let mut tracker = UltraTimeStopTracker::default();
        assert_eq!(
            tracker.events_from_packet(10.0, "CoolDown.Player.UltraSkill.F", &[], &table),
            vec![TimeStopEvent::ExtraStart {
                timestamp: 10.0,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            }]
        );

        let mut hit = targetless_hit();
        hit.timestamp = 10.2;
        hit.char_id = 1076;
        hit.ability_name = Some("GA_Shinku_UltraSkill".to_owned());
        assert_eq!(
            tracker.events_from_hits(&[hit], &table),
            vec![
                TimeStopEvent::ExtraEnd {
                    timestamp: 17.300015000000002,
                    reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
                },
                TimeStopEvent::UltraAnimation {
                    timestamp: 10.0,
                    char_id: 1076,
                    ability_id: "GA_Shinku_UltraSkill".to_owned(),
                    duration_seconds: 7.300015,
                },
            ]
        );

        assert!(
            tracker
                .events_from_packet(
                    20.0,
                    "CoolDown.Player.UltraSkill.Shinku.UltraPre",
                    &[],
                    &table,
                )
                .is_empty()
        );
        assert_eq!(
            tracker.events_from_packet(
                21.2,
                "CoolDown.Player.UltraSkill.Shinku.UltraRage",
                &[],
                &table,
            ),
            vec![TimeStopEvent::UltraAnimation {
                timestamp: 21.2,
                char_id: 1076,
                ability_id: "GA_Shinku_UltraSkill_Rage".to_owned(),
                duration_seconds: 5.300056,
            }]
        );
    }

    #[test]
    fn pending_ultra_cooldown_is_claimed_by_later_ultra_damage_hit() {
        let table = HashMap::from([(
            1010,
            UltraTimeStopEntry {
                ability_id: "GA_Nanally_UltraSkill".to_owned(),
                end_ability_event_seconds: 3.584608,
                source: "test".to_owned(),
                confidence: "high".to_owned(),
                ..UltraTimeStopEntry::default()
            },
        )]);
        let mut tracker = UltraTimeStopTracker::default();
        assert_eq!(
            tracker.events_from_packet(10.0, "CoolDown.Player.UltraSkill.F", &[], &table),
            vec![TimeStopEvent::ExtraStart {
                timestamp: 10.0,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            }]
        );
        let mut hit = targetless_hit();
        hit.timestamp = 12.0;
        hit.char_id = 1010;
        hit.ability_name = Some("GA_Nanally_UltraSkill".to_owned());
        let events = tracker.events_from_hits(&[hit], &table);

        assert_eq!(
            events,
            vec![
                TimeStopEvent::ExtraEnd {
                    timestamp: 13.584608,
                    reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
                },
                TimeStopEvent::UltraAnimation {
                    timestamp: 10.0,
                    char_id: 1010,
                    ability_id: "GA_Nanally_UltraSkill".to_owned(),
                    duration_seconds: 3.584608,
                },
            ]
        );
    }

    #[test]
    fn pending_ultra_cooldown_ignores_non_ultra_damage_before_claim() {
        let table = HashMap::from([
            (
                1010,
                UltraTimeStopEntry {
                    ability_id: "GA_Nanally_UltraSkill".to_owned(),
                    end_ability_event_seconds: 3.584608,
                    source: "test".to_owned(),
                    confidence: "high".to_owned(),
                    ..UltraTimeStopEntry::default()
                },
            ),
            (
                1052,
                UltraTimeStopEntry {
                    ability_id: "GA_Jin_UltraSkill".to_owned(),
                    end_ability_event_seconds: 2.183333,
                    source: "test".to_owned(),
                    confidence: "high".to_owned(),
                    ..UltraTimeStopEntry::default()
                },
            ),
        ]);
        let mut tracker = UltraTimeStopTracker::default();
        assert_eq!(
            tracker.events_from_packet(10.0, "CoolDown.Player.UltraSkill.F", &[], &table),
            vec![TimeStopEvent::ExtraStart {
                timestamp: 10.0,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            }]
        );
        let mut jin_skill_hit = targetless_hit();
        jin_skill_hit.timestamp = 10.5;
        jin_skill_hit.char_id = 1052;
        jin_skill_hit.ability_name = Some("GA_Jin_Skill".to_owned());
        assert!(
            tracker
                .events_from_hits(&[jin_skill_hit], &table)
                .is_empty()
        );

        let mut nanally_ultra_hit = targetless_hit();
        nanally_ultra_hit.timestamp = 12.0;
        nanally_ultra_hit.char_id = 1010;
        nanally_ultra_hit.ability_name = Some("GA_Nanally_UltraSkill".to_owned());
        assert_eq!(
            tracker.events_from_hits(&[nanally_ultra_hit], &table),
            vec![
                TimeStopEvent::ExtraEnd {
                    timestamp: 13.584608,
                    reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
                },
                TimeStopEvent::UltraAnimation {
                    timestamp: 10.0,
                    char_id: 1010,
                    ability_id: "GA_Nanally_UltraSkill".to_owned(),
                    duration_seconds: 3.584608,
                },
            ]
        );
    }

    #[test]
    fn pending_ultra_cooldown_claims_closest_prior_start() {
        let table = HashMap::from([(
            1010,
            UltraTimeStopEntry {
                ability_id: "GA_Nanally_UltraSkill".to_owned(),
                end_ability_event_seconds: 3.584608,
                source: "test".to_owned(),
                confidence: "high".to_owned(),
                ..UltraTimeStopEntry::default()
            },
        )]);
        let mut tracker = UltraTimeStopTracker::default();
        assert_eq!(
            tracker.events_from_packet(10.0, "CoolDown.Player.UltraSkill.F", &[], &table),
            vec![TimeStopEvent::ExtraStart {
                timestamp: 10.0,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            }]
        );
        assert_eq!(
            tracker.events_from_packet(11.5, "CoolDown.Player.UltraSkill.F", &[], &table),
            vec![TimeStopEvent::ExtraStart {
                timestamp: 11.5,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            }]
        );

        let mut hit = targetless_hit();
        hit.timestamp = 12.0;
        hit.char_id = 1010;
        hit.ability_name = Some("GA_Nanally_UltraSkill".to_owned());
        assert_eq!(
            tracker.events_from_hits(&[hit], &table),
            vec![
                TimeStopEvent::ExtraEnd {
                    timestamp: 15.084608,
                    reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
                },
                TimeStopEvent::UltraAnimation {
                    timestamp: 11.5,
                    char_id: 1010,
                    ability_id: "GA_Nanally_UltraSkill".to_owned(),
                    duration_seconds: 3.584608,
                },
            ]
        );

        assert_eq!(
            tracker.events_from_packet(15.0, "", &[], &table),
            vec![TimeStopEvent::ExtraEnd {
                timestamp: 10.0,
                reason: PENDING_ULTRA_TIME_STOP_REASON.to_owned(),
            }]
        );
    }

    #[test]
    fn ignored_packet_still_emits_pending_ultra_time_stop_end() {
        let (sender, receiver) = unbounded();
        let mut decoder = PacketDecoder {
            ultra_time_stops: HashMap::from([(
                1010,
                UltraTimeStopEntry {
                    ability_id: "GA_Nanally_UltraSkill".to_owned(),
                    end_ability_event_seconds: 3.584608,
                    source: "test".to_owned(),
                    confidence: "high".to_owned(),
                    ..UltraTimeStopEntry::default()
                },
            )]),
            ..PacketDecoder::default()
        };
        let characters = HashMap::new();
        let local_ip = Ipv4Addr::new(10, 0, 0, 2);
        let remote_ip = Ipv4Addr::new(10, 0, 0, 3);

        decoder.process_ethernet_frame(
            &udp_ipv4_packet(
                b"CoolDown.Player.UltraSkill.F",
                local_ip,
                50_000,
                remote_ip,
                7_777,
            ),
            10.0,
            Some(local_ip),
            true,
            &characters,
            &sender,
        );
        assert!(matches!(
            receiver
                .try_recv()
                .expect("pending start should be emitted"),
            EngineEvent::TimeStop(TimeStopEvent::ExtraStart {
                timestamp: 10.0,
                ..
            })
        ));
        assert!(matches!(
            receiver.try_recv().expect("debug packet should be emitted"),
            EngineEvent::Packet(_)
        ));

        decoder.process_ethernet_frame(
            &udp_ipv4_packet(&[0, 0, 0, 0], local_ip, 50_000, remote_ip, 7_777),
            15.0,
            Some(local_ip),
            true,
            &characters,
            &sender,
        );
        assert!(matches!(
            receiver.try_recv().expect("pending end should be emitted"),
            EngineEvent::TimeStop(TimeStopEvent::ExtraEnd {
                timestamp: 10.0,
                ..
            })
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn abyss_success_packet_can_also_identify_second_half_stage() {
        let events = abyss_events_from_text(
            30.0,
            "FAbyssGamePlayData\nConditionState_Success\nEAbyssFightStage::SecondHalf\nAbyssCloneCharacterItemData",
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            AbyssEvent::Stage {
                timestamp: 30.0,
                cycle: None,
                floor: None,
                half: AbyssHalf::Second,
                allow_late_backfill: true,
            }
        ));
        assert!(matches!(events[1], AbyssEvent::Success { timestamp: 30.0 }));
    }

    #[test]
    fn jin_packet_events_emit_extra_time_stop_interval_markers() {
        let mut tracker = UltraTimeStopTracker::default();
        let events = tracker.events_from_packet(
            12.0,
            "Event.Montage.Player.UltraSkill.Jin.EnterTimeStop\nEvent.Montage.Player.UltraSkill.Jin.ClearTimeStop",
            &[],
            &HashMap::new(),
        );

        assert_eq!(
            events,
            vec![
                TimeStopEvent::ExtraStart {
                    timestamp: 12.0,
                    reason: JIN_EXTRA_TIME_STOP_REASON.to_owned(),
                },
                TimeStopEvent::ExtraEnd {
                    timestamp: 12.0,
                    reason: JIN_EXTRA_TIME_STOP_REASON.to_owned(),
                },
            ]
        );
    }

    #[test]
    fn vehicle_hitout_is_outgoing_physical_damage() {
        let effects = [ParsedGameplayEffect {
            unique_index: 1845,
            byte_offset: 0,
            bit_shift: 0,
        }];
        let names = HashMap::from([(1845, "GE_Vehicle_HitOut2".to_owned())]);
        let mut hit = targetless_hit();
        hit.direction = "incoming".to_owned();

        enrich_hit_with_gameplay_effect(
            &mut hit,
            &effects,
            &names,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(hit.direction, "outgoing");
        assert_eq!(hit.attack_type.as_deref(), Some("载具伤害"));
        assert_eq!(hit.damage_attribute.as_deref(), Some("物理"));
    }

    #[test]
    fn player_damage_effect_is_outgoing_even_when_record_looks_incoming() {
        let effects = [ParsedGameplayEffect {
            unique_index: 2031,
            byte_offset: 0,
            bit_shift: 0,
        }];
        let names = HashMap::from([(2031, "GE_Player_Hathor_QTE1_Damage".to_owned())]);
        let mut hit = targetless_hit();
        hit.direction = "incoming".to_owned();

        enrich_hit_with_gameplay_effect(
            &mut hit,
            &effects,
            &names,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(hit.direction, "outgoing");
        assert_eq!(hit.attack_type.as_deref(), Some("环合"));
    }

    #[test]
    fn non_prefixed_player_skill_damage_effect_overrides_incoming_direction() {
        let effects = [ParsedGameplayEffect {
            unique_index: 3010,
            byte_offset: 0,
            bit_shift: 0,
        }];
        let names = HashMap::from([(3010, "GE_Nanally010_Lv3_Damage".to_owned())]);
        let skills = HashMap::from([(
            "GE_Nanally010_Lv3_Damage".to_owned(),
            GameplayEffectSkill {
                damage_source_category: Some("A".to_owned()),
                ability_name: Some("GA_Nanally_Melee".to_owned()),
                attack_type: "普攻".to_owned(),
            },
        )]);
        let mut hit = targetless_hit();
        hit.direction = "incoming".to_owned();

        enrich_hit_with_gameplay_effect(&mut hit, &effects, &names, &skills, &HashMap::new());

        assert_eq!(hit.direction, "outgoing");
        assert_eq!(hit.attack_type.as_deref(), Some("普攻"));
    }

    #[test]
    fn numeric_ability_owner_overrides_final_tower_packet_id() {
        let characters = HashMap::from([
            (1019, character_with_attribute("薄荷", "灵")),
            (1076, character_with_attribute("真红", "光")),
        ]);
        let mut hit = targetless_hit();
        hit.char_id = 1076;
        hit.char_name = "真红".to_owned();
        hit.char_source = "packet".to_owned();
        hit.ability_name = Some("GA_Mint019_Skill".to_owned());

        reattribute_hit_from_ability_name(&mut hit, true, &characters);

        assert_eq!(hit.char_id, 1019);
        assert_eq!(hit.char_name, "薄荷");
        assert_eq!(hit.char_source, "gameplay_effect");
    }

    #[test]
    fn numeric_ability_owner_keeps_regular_packet_id() {
        let characters = HashMap::from([
            (1019, character_with_attribute("薄荷", "灵")),
            (1076, character_with_attribute("真红", "光")),
        ]);
        let mut hit = targetless_hit();
        hit.char_id = 1076;
        hit.char_name = "真红".to_owned();
        hit.char_source = "packet".to_owned();
        hit.ability_name = Some("GA_Mint019_Skill".to_owned());

        reattribute_hit_from_ability_name(&mut hit, false, &characters);

        assert_eq!(hit.char_id, 1076);
        assert_eq!(hit.char_name, "真红");
        assert_eq!(hit.char_source, "packet");
    }

    #[test]
    fn numeric_ability_owner_overrides_session_id() {
        let characters = HashMap::from([
            (1019, character_with_attribute("薄荷", "灵")),
            (1076, character_with_attribute("真红", "光")),
        ]);
        let mut hit = targetless_hit();
        hit.char_id = 1076;
        hit.char_name = "真红".to_owned();
        hit.char_source = "session".to_owned();
        hit.ability_name = Some("GA_Mint019_QTE".to_owned());

        reattribute_hit_from_ability_name(&mut hit, false, &characters);

        assert_eq!(hit.char_id, 1019);
        assert_eq!(hit.char_name, "薄荷");
        assert_eq!(hit.char_source, "gameplay_effect");
    }

    #[test]
    fn reaction_damage_effect_overrides_incoming_direction() {
        let effects = [ParsedGameplayEffect {
            unique_index: 4010,
            byte_offset: 0,
            bit_shift: 0,
        }];
        let names = HashMap::from([(4010, "GE_ActorReaction_1_Damage".to_owned())]);
        let mut hit = targetless_hit();
        hit.direction = "incoming".to_owned();

        enrich_hit_with_gameplay_effect(
            &mut hit,
            &effects,
            &names,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(hit.direction, "outgoing");
        assert_eq!(hit.attack_type.as_deref(), Some("创生花"));
    }

    #[test]
    fn reaction_buff_effect_overrides_incoming_direction() {
        let effects = [ParsedGameplayEffect {
            unique_index: 4011,
            byte_offset: 0,
            bit_shift: 0,
        }];
        let names = HashMap::from([(4011, "Buff_Reaction_4_new".to_owned())]);
        let mut hit = targetless_hit();
        hit.direction = "incoming".to_owned();

        enrich_hit_with_gameplay_effect(
            &mut hit,
            &effects,
            &names,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(hit.direction, "outgoing");
        assert_eq!(hit.attack_type.as_deref(), Some("黯星"));
    }

    #[test]
    fn tenacity_damage_effect_overrides_incoming_direction() {
        let effects = [ParsedGameplayEffect {
            unique_index: 4012,
            byte_offset: 0,
            bit_shift: 0,
        }];
        let names = HashMap::from([(4012, "Buff_Tenacity_damage".to_owned())]);
        let mut hit = targetless_hit();
        hit.direction = "incoming".to_owned();

        enrich_hit_with_gameplay_effect(
            &mut hit,
            &effects,
            &names,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(hit.direction, "outgoing");
        assert_eq!(hit.attack_type.as_deref(), Some("倾陷伤害"));
    }

    #[test]
    fn monster_damage_effect_overrides_outgoing_direction_to_incoming() {
        let effects = [ParsedGameplayEffect {
            unique_index: 5010,
            byte_offset: 0,
            bit_shift: 0,
        }];
        let names = HashMap::from([(5010, "GE_mon_25_act05_Dmg02_BP".to_owned())]);
        let mut hit = targetless_hit();
        hit.direction = "outgoing".to_owned();

        enrich_hit_with_gameplay_effect(
            &mut hit,
            &effects,
            &names,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(hit.direction, "incoming");
        assert_eq!(hit.attack_type.as_deref(), Some("其他"));
    }

    #[test]
    fn monster_steal_damage_effect_does_not_force_incoming_direction() {
        let effects = [ParsedGameplayEffect {
            unique_index: 5010,
            byte_offset: 0,
            bit_shift: 0,
        }];
        let names = HashMap::from([(5010, "GE_mon_14_act05_Dmg01_Steal_BP".to_owned())]);
        let skills = HashMap::from([(
            "GE_mon_14_act05_Dmg01_Steal_BP".to_owned(),
            GameplayEffectSkill {
                damage_source_category: Some("E".to_owned()),
                ability_name: Some("GA_Lacrimosa_Skill".to_owned()),
                attack_type: "E技能".to_owned(),
            },
        )]);
        let mut hit = targetless_hit();
        hit.direction = "outgoing".to_owned();

        enrich_hit_with_gameplay_effect(&mut hit, &effects, &names, &skills, &HashMap::new());

        assert_eq!(hit.direction, "outgoing");
        assert_eq!(hit.attack_type.as_deref(), Some("E技能"));
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
            damage_attribute: None,
            follow_up_damage: 0.0,
            follow_up_timestamp: None,
            follow_up_damage_name: None,
            follow_up_attack_type: None,
            follow_up_damage_attribute: None,
        }
    }

    fn udp_ipv4_packet(
        payload: &[u8],
        src: Ipv4Addr,
        src_port: u16,
        dst: Ipv4Addr,
        dst_port: u16,
    ) -> Vec<u8> {
        let ip_len = 20 + 8 + payload.len();
        let udp_len = 8 + payload.len();
        let mut packet = Vec::with_capacity(14 + ip_len);
        packet.extend_from_slice(&[0, 1, 2, 3, 4, 5]);
        packet.extend_from_slice(&[6, 7, 8, 9, 10, 11]);
        packet.extend_from_slice(&0x0800_u16.to_be_bytes());
        packet.push(0x45);
        packet.push(0);
        packet.extend_from_slice(&(ip_len as u16).to_be_bytes());
        packet.extend_from_slice(&0_u16.to_be_bytes());
        packet.extend_from_slice(&0_u16.to_be_bytes());
        packet.push(64);
        packet.push(17);
        packet.extend_from_slice(&0_u16.to_be_bytes());
        packet.extend_from_slice(&src.octets());
        packet.extend_from_slice(&dst.octets());
        packet.extend_from_slice(&src_port.to_be_bytes());
        packet.extend_from_slice(&dst_port.to_be_bytes());
        packet.extend_from_slice(&(udp_len as u16).to_be_bytes());
        packet.extend_from_slice(&0_u16.to_be_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    fn follow_up_test_characters() -> HashMap<u32, CharacterInfo> {
        HashMap::from([
            (
                1,
                CharacterInfo {
                    name_zh: "ling".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: None,
                    attribute: Some("灵".to_owned()),
                },
            ),
            (
                2,
                CharacterInfo {
                    name_zh: "zhou".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: None,
                    attribute: Some("咒".to_owned()),
                },
            ),
            (
                3,
                CharacterInfo {
                    name_zh: "other".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: None,
                    attribute: Some("光".to_owned()),
                },
            ),
        ])
    }

    fn observe_visible_fuwen_trigger(
        tracker: &mut FollowUpDamageTracker,
        character_id: u32,
        timestamp: f64,
    ) {
        let mut hit = targetless_hit();
        hit.char_id = character_id;
        hit.timestamp = timestamp;
        hit.attack_type = Some("环合·覆纹".to_owned());
        tracker.observe_fuwen_trigger_hit(&hit);
    }

    fn character_evidence_row(character_id: u32) -> [u8; 9] {
        let digits = format!("{character_id:04}");
        let mut row = [0_u8; 9];
        row[..4].copy_from_slice(&[5, 0, 0, 0]);
        row[4..8].copy_from_slice(digits.as_bytes());
        row
    }

    fn write_shifted_bytes(payload: &mut [u8], bit_shift: u8, byte_offset: usize, bytes: &[u8]) {
        for (index, byte) in bytes.iter().enumerate() {
            for bit in 0..8 {
                let bit_value = (byte >> bit) & 1;
                let target_bit = bit_shift as usize + (byte_offset + index) * 8 + bit;
                let target_byte = target_bit / 8;
                let target_bit_offset = target_bit % 8;
                if bit_value == 1 {
                    payload[target_byte] |= 1 << target_bit_offset;
                } else {
                    payload[target_byte] &= !(1 << target_bit_offset);
                }
            }
        }
    }

    fn duplicate_test_hit(timestamp: f64, char_source: &str, direction: &str) -> Hit {
        let mut hit = targetless_hit();
        hit.timestamp = timestamp;
        hit.char_id = if char_source == "packet" { 1051 } else { 1010 };
        hit.char_name = if char_source == "packet" {
            "零(女)".to_owned()
        } else {
            "娜娜莉".to_owned()
        };
        hit.damage = 5_829.0;
        hit.char_source = char_source.to_owned();
        hit.direction = direction.to_owned();
        hit.target_hp_before = 1_389_577.0;
        hit.target_hp_after = 1_383_748.0;
        hit.target_max_hp = 1_930_389.0;
        hit.gameplay_effect_index = Some(52);
        hit.gameplay_effect_name = Some("GE_ActorReaction_1_Damage".to_owned());
        hit.attack_type = Some("创生花".to_owned());
        hit
    }

    fn duplicate_test_characters() -> HashMap<u32, CharacterInfo> {
        HashMap::from([
            (
                1010,
                CharacterInfo {
                    name_zh: "娜娜莉".to_owned(),
                    name_en: "Nanally".to_owned(),
                    color: None,
                    avatar: None,
                    attribute: Some("咒".to_owned()),
                },
            ),
            (
                1020,
                CharacterInfo {
                    name_zh: "哈尼娅".to_owned(),
                    name_en: "Haniel".to_owned(),
                    color: None,
                    avatar: None,
                    attribute: Some("光".to_owned()),
                },
            ),
            (
                1051,
                CharacterInfo {
                    name_zh: "零(女)".to_owned(),
                    name_en: "Rei".to_owned(),
                    color: None,
                    avatar: None,
                    attribute: Some("灵".to_owned()),
                },
            ),
            (
                1055,
                CharacterInfo {
                    name_zh: "测试角色".to_owned(),
                    name_en: "Test".to_owned(),
                    color: None,
                    avatar: None,
                    attribute: None,
                },
            ),
        ])
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

    fn boss_hp_update(timestamp_hp: f32) -> crate::parser::ParsedBossHpUpdate {
        crate::parser::ParsedBossHpUpdate {
            target_handle: [7; 16],
            current_hp: timestamp_hp,
            byte_offset: 0,
            bit_shift: 0,
        }
    }

    #[test]
    fn server_damage_calibration_corrects_single_pending_hit() {
        let mut tracker = ServerDamageCalibrationTracker::default();
        assert!(
            tracker
                .observe_boss_hp(9.0, &boss_hp_update(10_000.0))
                .is_none()
        );
        let mut hit = duplicate_test_hit(10.0, "packet", "outgoing");
        hit.damage = 1_000.0;
        hit.target_hp_before = 10_000.0;
        hit.target_hp_after = 9_000.0;
        hit.target_max_hp = 10_000.0;
        tracker.observe_hit(&hit);

        let correction = tracker
            .observe_boss_hp(10.05, &boss_hp_update(8_750.0))
            .expect("single pending hit should use server HP delta");

        assert_eq!(correction.source_damage, 1_000.0);
        assert_eq!(correction.damage, 1_250.0);
        assert_eq!(correction.target_hp_before, 10_000.0);
        assert_eq!(correction.target_hp_after, 8_750.0);
    }

    #[test]
    fn server_damage_calibration_does_not_split_multiple_pending_hits() {
        let mut tracker = ServerDamageCalibrationTracker::default();
        assert!(
            tracker
                .observe_boss_hp(9.0, &boss_hp_update(10_000.0))
                .is_none()
        );
        let mut first = duplicate_test_hit(10.0, "packet", "outgoing");
        first.target_hp_before = 10_000.0;
        first.target_hp_after = 9_000.0;
        first.target_max_hp = 10_000.0;
        let mut second = duplicate_test_hit(10.02, "packet", "outgoing");
        second.target_hp_before = 9_000.0;
        second.target_hp_after = 8_500.0;
        second.target_max_hp = 10_000.0;
        tracker.observe_hit(&first);
        tracker.observe_hit(&second);

        assert!(
            tracker
                .observe_boss_hp(10.05, &boss_hp_update(8_500.0))
                .is_none()
        );
    }

    #[test]
    fn confirmed_packet_hit_suppresses_ambiguous_session_candidate() {
        let mut decoder = PacketDecoder::default();
        let candidate = duplicate_test_hit(10.0, "session", "unknown");

        let characters = duplicate_test_characters();

        let prepared =
            decoder.prepare_hits_for_emission(vec![candidate], &[1051, 1055], false, &characters);

        assert!(prepared.emit.is_empty());
        assert_eq!(prepared.deferred_ambiguous, 1);
        assert_eq!(decoder.pending_ambiguous_hits.len(), 1);

        let confirmed = duplicate_test_hit(10.1, "packet", "outgoing");
        let prepared =
            decoder.prepare_hits_for_emission(vec![confirmed], &[1051], false, &characters);

        assert_eq!(prepared.emit.len(), 1);
        assert_eq!(prepared.suppressed_ambiguous, 1);
        assert_eq!(prepared.emit[0].char_id, 1051);
        assert!(decoder.pending_ambiguous_hits.is_empty());
    }

    #[test]
    fn confirmed_packet_hit_suppresses_recent_duplicate_records() {
        let mut decoder = PacketDecoder::default();
        let characters = duplicate_test_characters();
        let confirmed = duplicate_test_hit(10.0, "packet", "outgoing");
        let mut duplicate = confirmed.clone();
        duplicate.gameplay_effect_index = None;
        duplicate.gameplay_effect_name = None;
        duplicate.attack_type = None;
        duplicate.target_hp_before += 2_000.0;
        duplicate.target_hp_after += 2_000.0;

        let prepared = decoder.prepare_hits_for_emission(
            vec![confirmed, duplicate],
            &[1051],
            true,
            &characters,
        );

        assert_eq!(prepared.emit.len(), 1);
        assert_eq!(prepared.suppressed_ambiguous, 1);
        assert_eq!(prepared.emit[0].attack_type.as_deref(), Some("创生花"));
    }

    #[test]
    fn boss_hp_sync_damage_merges_into_recent_confirmed_hit() {
        let mut decoder = PacketDecoder::default();
        let characters = duplicate_test_characters();
        let mut source = duplicate_test_hit(10.0, "packet", "outgoing");
        source.damage = 26_185.0;
        source.target_hp_before = 29_700.0;
        source.target_hp_after = 3_515.0;
        source.target_max_hp = 1_930_389.0;

        let prepared = decoder.prepare_hits_for_emission(vec![source], &[1051], false, &characters);
        assert_eq!(prepared.emit.len(), 1);

        let follow_up = decoder
            .infer_boss_hp_sync_damage(10.04, 1.0, &characters)
            .expect("server HP sync should add the missing lethal delta");

        assert_eq!(follow_up.damage, 3_515.0);
        assert_eq!(follow_up.target_hp_after, 0.0);
        assert_eq!(follow_up.damage_name.as_deref(), Some("HP同步伤害"));
        assert_eq!(follow_up.source_char_id, 1051);
    }

    #[test]
    fn nonlethal_boss_hp_sync_does_not_merge_into_recent_confirmed_hit() {
        let mut decoder = PacketDecoder::default();
        let characters = duplicate_test_characters();
        let mut source = duplicate_test_hit(10.0, "packet", "outgoing");
        source.damage = 8_446.0;
        source.target_hp_after = 1_081_975.0;
        source.target_max_hp = 1_930_389.0;

        let prepared = decoder.prepare_hits_for_emission(vec![source], &[1051], false, &characters);
        assert_eq!(prepared.emit.len(), 1);

        assert!(
            decoder
                .infer_boss_hp_sync_damage(10.04, 1_057_660.0, &characters)
                .is_none()
        );
    }

    #[test]
    fn gameplay_effect_name_confirms_session_hit_in_multi_character_packet() {
        let mut decoder = PacketDecoder::default();
        let characters = duplicate_test_characters();
        let mut hit = targetless_hit();
        hit.timestamp = 10.0;
        hit.char_id = 1020;
        hit.char_name = "哈尼娅".to_owned();
        hit.char_source = "session".to_owned();
        hit.direction = "unknown".to_owned();
        hit.damage = 6_961.0;
        hit.gameplay_effect_index = Some(3261);
        hit.gameplay_effect_name = Some("GE_Player_Haniel_Skill1_Damage".to_owned());
        hit.attack_type = Some("E技能".to_owned());

        let prepared =
            decoder.prepare_hits_for_emission(vec![hit], &[1010, 1020], false, &characters);

        assert_eq!(prepared.emit.len(), 1);
        assert_eq!(prepared.deferred_ambiguous, 0);
        assert_eq!(prepared.emit[0].direction, "outgoing");
        assert_eq!(prepared.emit[0].char_source, "gameplay_effect");
        assert!(decoder.pending_ambiguous_hits.is_empty());
    }

    #[test]
    fn ambiguous_session_candidate_expires_when_unconfirmed() {
        let mut decoder = PacketDecoder::default();
        let candidate = duplicate_test_hit(10.0, "session", "unknown");

        let characters = duplicate_test_characters();

        decoder.prepare_hits_for_emission(vec![candidate], &[1051, 1055], false, &characters);

        assert!(decoder.take_expired_ambiguous_hits(10.25).is_empty());
        let expired = decoder.take_expired_ambiguous_hits(10.75);

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].char_source, "session");
        assert!(decoder.pending_ambiguous_hits.is_empty());
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
    fn resolves_explode_damage_name_from_base_effect() {
        let skills = HashMap::from([(
            "GE_Player_Lacrimosa_B_Melee3_Explode_Damage".to_owned(),
            GameplayEffectSkill {
                damage_source_category: Some("A".to_owned()),
                ability_name: Some("GA_Lacrimosa_Melee".to_owned()),
                attack_type: "普攻".to_owned(),
            },
        )]);
        let names = HashMap::from([(
            "GE_Player_Lacrimosa_B_Melee3_Damage".to_owned(),
            "安魂曲普攻B".to_owned(),
        )]);

        assert_eq!(
            resolve_damage_name(
                "GE_Player_Lacrimosa_B_Melee3_Explode_Damage",
                &skills,
                &names
            )
            .as_deref(),
            Some("安魂曲普攻B")
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

    #[test]
    #[ignore = "set NTE_TEST_CAPTURE to a local pcapng path for capture diagnostics"]
    fn diagnose_capture_time_stop_events() {
        let path = std::env::var("NTE_TEST_CAPTURE").expect("NTE_TEST_CAPTURE must be set");
        let characters = Arc::new(
            load_characters(Path::new(CHARACTER_DATA_PATH))
                .expect("character resource table should load"),
        );
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = import_pcapng(
            PathBuf::from(path),
            characters,
            None,
            true,
            true,
            sender,
            stop,
        );
        handle.join().expect("pcapng import thread should finish");

        let mut shinku_hits = Vec::new();
        let mut time_stops = Vec::new();
        let mut relevant_packets = Vec::new();
        let mut statuses = Vec::new();
        let mut warnings = Vec::new();
        let mut errors = Vec::new();
        for event in receiver.try_iter() {
            match event {
                EngineEvent::Hit(hit) if hit.char_id == 1076 => shinku_hits.push(*hit),
                EngineEvent::Packet(packet)
                    if packet.decoded_text.contains("Shinku")
                        || packet.decoded_text.contains("UltraSkill")
                        || packet.decoded_text.contains("1076") =>
                {
                    relevant_packets.push(*packet);
                }
                EngineEvent::TimeStop(event) => time_stops.push(event),
                EngineEvent::Status(status) => statuses.push(status),
                EngineEvent::Warning(warning) => warnings.push(warning),
                EngineEvent::Error(error) => errors.push(error),
                _ => {}
            }
        }

        println!("statuses: {statuses:#?}");
        println!("warnings: {warnings:#?}");
        println!("errors: {errors:#?}");
        println!("shinku hit count: {}", shinku_hits.len());
        for hit in &shinku_hits {
            println!(
                "shinku hit t={:.6} damage={:.1} ability={:?} effect={:?} attack={:?}",
                hit.timestamp,
                hit.damage,
                hit.ability_name,
                hit.gameplay_effect_name,
                hit.attack_type
            );
        }
        println!("time stop count: {}", time_stops.len());
        for event in &time_stops {
            println!("time stop: {event:?}");
        }
        println!("relevant packet count: {}", relevant_packets.len());
        for packet in &relevant_packets {
            let text = packet
                .decoded_text
                .lines()
                .filter(|line| {
                    line.contains("Shinku")
                        || line.contains("UltraSkill")
                        || line.contains("TimeStop")
                        || line.contains("EndAbility")
                        || line.contains("CoolDown")
                })
                .take(12)
                .collect::<Vec<_>>()
                .join(" | ");
            println!(
                "packet t={:.6} dir={} ids={:?} hits={} text={}",
                packet.timestamp, packet.direction, packet.declared_ids, packet.parsed_hits, text
            );
        }
    }
}
