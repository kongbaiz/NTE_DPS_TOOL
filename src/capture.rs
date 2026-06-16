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

use crate::model::{AbyssEvent, AbyssHalf, CharacterInfo, EngineEvent, Hit, PacketDebug};
use crate::parser::{
    GAMEPLAY_EFFECT_MAPPING_PATH, GameplayEffectSkill, ParsedGameplayEffect,
    SKILL_DAMAGE_DATA_PATH, WOODEN_DAMAGE_DESCRIPTIONS_PATH, classify_attack_type,
    classify_attack_type_from_description, declared_character_ids_from_evidence, find_data_file,
    find_declared_character_evidence, load_gameplay_effect_mapping, load_gameplay_effect_skills,
    load_wooden_damage_names, normalize_damage_name, parse_boss_hp_updates,
    parse_current_hp_updates, parse_damage_payload, parse_gameplay_effects, qte_reaction_type,
};

use crate::protocol::{TransportPacket, parse_single_bunch, parse_transport_packet};

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
    follow_up_damage: FollowUpDamageTracker,
    character_declarations: HashMap<u32, f64>,
    resource_warnings: Vec<String>,
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
            follow_up_damage: FollowUpDamageTracker::default(),
            character_declarations: HashMap::new(),
            resource_warnings,
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
        if current_hp_updates.is_empty()
            && boss_hp_updates.is_empty()
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
        let inferred_follow_up_hits = boss_hp_updates
            .iter()
            .filter_map(|update| {
                self.follow_up_damage
                    .observe_server_hp(timestamp, update.current_hp as f64)
            })
            .collect::<Vec<_>>();
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
        for hit in hits {
            if include_incoming || hit.direction != "incoming" {
                self.follow_up_damage
                    .observe_hit(&hit, hit.gameplay_effect_index, characters);
                let _ = sender.send(EngineEvent::Hit(hit));
            }
        }
        for hit in inferred_follow_up_hits {
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
        hex::decode(&packet.payload_hex).map_err(|error| format!("payload_hex 鏃犳晥: {error}"))?
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
            } else if let Some(reaction_type) = attack_type
                .strip_prefix("QTE·")
                .or_else(|| attack_type.strip_prefix("QTE路"))
            {
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
                .contains("payload_hex 鏃犳晥")
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
            decoded_text: "娴嬭瘯瑙ｇ爜".to_owned(),
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
        }
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
