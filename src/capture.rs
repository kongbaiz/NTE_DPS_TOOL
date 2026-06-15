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
    classify_attack_type_from_description, declared_character_ids_from_evidence,
    detect_monster_targets, find_data_file, find_declared_character_evidence,
    load_gameplay_effect_mapping, load_gameplay_effect_skills, load_wooden_damage_names,
    monster_target_from_identifier, normalize_damage_name, parse_boss_hp_updates,
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
            .map_err(|_| "原始抓包状态不可用".to_owned())?;
        if capture.writer.is_some() {
            return Err("抓包文件仍在写入，请先停止抓包".to_owned());
        }
        if let Some(error) = &capture.write_error {
            return Err(format!("原始抓包写入失败：{error}"));
        }
        if path != capture.path {
            std::fs::copy(&capture.path, path).map_err(|error| {
                format!(
                    "无法复制抓包文件 {} 到 {}: {error}",
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
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("无法创建原始抓包目录 {}: {error}", parent.display()))?;
        }
        let file = File::create(path)
            .map_err(|error| format!("无法创建原始抓包文件 {}: {error}", path.display()))?;
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
            "已识别 UE 长度前缀角色声明；其余内容为无内联字段名的位打包复制增量，\
检测到 {} 个锚点、{} 种位对齐：{}",
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
        "候选 UE 位打包复制/引用增量：无 FString 锚点或内联字段名，\
零字节占比 {:.1}%，熵 {:.2} bit/byte",
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
        note.push('；');
    }
    note.push_str(&diagnostic);
}

fn parse_abyss_stage_id(value: &str) -> Option<(u32, AbyssHalf)> {
    let parts: Vec<_> = value.split('_').collect();
    if parts.len() < 4 || parts.first().copied() != Some("Abyss") {
        return None;
    }
    let floor = parts.get(parts.len() - 2)?.parse().ok()?;
    let half = match *parts.last()? {
        "0" => AbyssHalf::First,
        "1" => AbyssHalf::Second,
        _ => return None,
    };
    Some((floor, half))
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
    if let Some((floor, half)) = explicit_stage {
        events.push(AbyssEvent::Stage {
            timestamp,
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
    for scene in crate::scene::detect_scenes(packet.timestamp, &packet.decoded_text) {
        let _ = sender.send(EngineEvent::Scene(scene));
    }
    let _ = sender.send(EngineEvent::Packet(packet));
}

const INFERRED_FOLLOW_UP_CHAR_ID: u32 = u32::MAX;
const NANALLY_MELEE1_GAMEPLAY_EFFECT_INDEX: u32 = 241;
const MAX_DISPLAY_DAMAGE_CORRECTION_RATIO: f64 = 0.03;

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
            target_id: source.target_id,
            target_name: source.target_name,
            target_context: vec![
                format!(
                    "服务器实际掉血 {:.0} - 封包伤害 {:.0}（残差 {:.1}%）",
                    actual_damage,
                    source.damage,
                    inferred_ratio * 100.0
                ),
                if corrected {
                    format!(
                        "GameplayEffect {}：按显示伤害的 20% 反解覆纹 {:.0}",
                        pending.gameplay_effect_index.unwrap_or_default(),
                        inferred_damage
                    )
                } else {
                    "覆纹取服务器 HP 残差".to_owned()
                },
                format!(
                    "触发角色 {}（{}，{}属性）",
                    source.char_name, source.char_id, source_attribute
                ),
            ],
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
    follow_up_damage: FollowUpDamageTracker,
    gameplay_effect_names: HashMap<u32, String>,
    gameplay_effect_skills: HashMap<String, GameplayEffectSkill>,
    wooden_damage_names: HashMap<String, String>,
    resource_warnings: Vec<String>,
    character_declarations: HashMap<u32, f64>,
    current_monster: Option<InferredMonsterTarget>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MonsterTargetSource {
    Entity,
    GameplayEffect,
}

impl MonsterTargetSource {
    fn confidence(self) -> u8 {
        match self {
            Self::Entity => 1,
            Self::GameplayEffect => 2,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Entity => "怪物实体特征",
            Self::GameplayEffect => "GameplayEffect 特征",
        }
    }

    fn confidence_label(self) -> &'static str {
        match self {
            Self::Entity => "低",
            Self::GameplayEffect => "高",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct InferredMonsterTarget {
    id: String,
    name: String,
    source: MonsterTargetSource,
    observed_at: f64,
    expires_at: f64,
}

impl Default for PacketDecoder {
    fn default() -> Self {
        let mapping_relative = Path::new(GAMEPLAY_EFFECT_MAPPING_PATH);
        let skills_relative = Path::new(SKILL_DAMAGE_DATA_PATH);
        let wooden_relative = Path::new(WOODEN_DAMAGE_DESCRIPTIONS_PATH);
        let mut resource_warnings = Vec::new();
        let gameplay_effect_names = find_data_file(mapping_relative)
            .ok_or_else(|| format!("找不到 {}", mapping_relative.display()))
            .and_then(|path| load_gameplay_effect_mapping(&path).map_err(|error| error.to_string()))
            .unwrap_or_else(|error| {
                resource_warnings.push(format!("GameplayEffect 名称表加载失败：{error}"));
                HashMap::new()
            });
        let gameplay_effect_skills = find_data_file(skills_relative)
            .ok_or_else(|| format!("找不到 {}", skills_relative.display()))
            .and_then(|path| load_gameplay_effect_skills(&path).map_err(|error| error.to_string()))
            .unwrap_or_else(|error| {
                resource_warnings.push(format!("技能分类表加载失败：{error}"));
                HashMap::new()
            });
        let wooden_damage_names = find_data_file(wooden_relative)
            .ok_or_else(|| format!("找不到 {}", wooden_relative.display()))
            .and_then(|path| load_wooden_damage_names(&path).map_err(|error| error.to_string()))
            .unwrap_or_else(|error| {
                resource_warnings.push(format!("木桩伤害描述表加载失败：{error}"));
                HashMap::new()
            });
        Self {
            session_characters: HashMap::new(),
            client_endpoints: HashSet::new(),
            follow_up_damage: FollowUpDamageTracker::default(),
            gameplay_effect_names,
            gameplay_effect_skills,
            wooden_damage_names,
            resource_warnings,
            character_declarations: HashMap::new(),
            current_monster: None,
        }
    }
}

impl PacketDecoder {
    fn observe_monster(
        &mut self,
        id: String,
        name: String,
        source: MonsterTargetSource,
        observed_at: f64,
        expires_at: f64,
    ) {
        let should_replace = self.current_monster.as_ref().is_none_or(|current| {
            id == current.id
                || source.confidence() >= current.source.confidence()
                || observed_at >= current.expires_at
        });
        if should_replace {
            self.current_monster = Some(InferredMonsterTarget {
                id,
                name,
                source,
                observed_at,
                expires_at,
            });
        }
    }

    fn inferred_monster(
        &self,
        timestamp: f64,
        packet_has_multiple_targets: bool,
    ) -> Option<&InferredMonsterTarget> {
        if packet_has_multiple_targets {
            return None;
        }
        self.current_monster
            .as_ref()
            .filter(|target| timestamp >= target.observed_at && timestamp <= target.expires_at)
    }

    fn apply_inferred_monster(
        &self,
        hits: &mut [Hit],
        timestamp: f64,
        packet_has_multiple_targets: bool,
    ) {
        let Some(target) = self.inferred_monster(timestamp, packet_has_multiple_targets) else {
            return;
        };
        for hit in hits {
            if hit.target_name.is_none() {
                hit.target_id = Some(target.id.clone());
                hit.target_name = Some(target.name.clone());
                hit.target_context.push(format!(
                    "推断目标（非协议直接命中）：来源 {}，置信度{}",
                    target.source.label(),
                    target.source.confidence_label()
                ));
            }
        }
    }

    fn resource_warning(&self) -> Option<String> {
        (!self.resource_warnings.is_empty()).then(|| self.resource_warnings.join("；"))
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
        let detected_monsters = detect_monster_targets(payload);
        let mut effect_monsters = gameplay_effects
            .iter()
            .filter_map(|effect| self.gameplay_effect_names.get(&effect.unique_index))
            .filter_map(|effect_name| monster_target_from_identifier(effect_name))
            .collect::<Vec<_>>();
        effect_monsters.sort();
        effect_monsters.dedup();
        let mut packet_target_ids = detected_monsters
            .iter()
            .chain(effect_monsters.iter())
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>();
        packet_target_ids.sort_unstable();
        packet_target_ids.dedup();
        let packet_has_multiple_targets = packet_target_ids.len() > 1;
        if !packet_has_multiple_targets {
            if effect_monsters.len() == 1 {
                let (id, name) = effect_monsters.remove(0);
                self.observe_monster(
                    id,
                    name,
                    MonsterTargetSource::GameplayEffect,
                    timestamp,
                    timestamp + 3.0,
                );
            } else if detected_monsters.len() == 1 {
                let (id, name) = detected_monsters[0].clone();
                self.observe_monster(
                    id,
                    name,
                    MonsterTargetSource::Entity,
                    timestamp,
                    timestamp + 0.25,
                );
            }
        }
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
        let decoded_text = decode_payload_text(payload);
        let scene_transition = crate::scene::detect_scenes(timestamp, &decoded_text)
            .iter()
            .any(|scene| scene.category == "transition");
        if scene_transition {
            self.current_monster = None;
        } else if outgoing {
            self.apply_inferred_monster(&mut hits, timestamp, packet_has_multiple_targets);
        }
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
                    hit.target_context.push(format!(
                        "异能环合：{}({}) + {}({}) = {}",
                        previous_declared_character
                            .and_then(|character_id| characters.get(&character_id))
                            .map(|character| character.name_zh.as_str())
                            .filter(|name| !name.is_empty())
                            .unwrap_or("前台角色"),
                        previous_attribute,
                        hit.char_name,
                        entering_attribute,
                        reaction_type
                    ));
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
                    "GameplayEffect：{}",
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
                    "Boss HP 更新：{}",
                    boss_hp_updates
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
            return Err(format!("打开网卡失败: {}", c_string(error_buffer.as_ptr())));
        }

        let capture_filter = CString::new(filter).map_err(|error| error.to_string())?;
        let mut program = BpfProgram {
            bf_len: 0,
            bf_insns: ptr::null_mut(),
        };
        if compile(handle, &mut program, capture_filter.as_ptr(), 1, u32::MAX) != 0
            || set_filter(handle, &mut program) != 0
        {
            let error = c_string(get_err(handle));
            free_code(&mut program);
            close(handle);
            return Err(format!("抓包过滤器无效: {error}"));
        }
        free_code(&mut program);
        let raw_capture_status = raw_capture.path().map_or_else(
            || "；原始帧文件不可用".to_owned(),
            |path| format!("；原始帧实时写入 {}", path.display()),
        );
        let _ = sender.send(EngineEvent::Status(format!(
            "正在抓包: {} ({}){}",
            device.description,
            local_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "不过滤本机 IP".to_owned()),
            raw_capture_status
        )));

        let mut decoder = PacketDecoder::default();
        if let Some(warning) = decoder.resource_warning() {
            let _ = sender.send(EngineEvent::Warning(warning));
        }
        while !stop.load(Ordering::Relaxed) {
            let mut header = ptr::null();
            let mut packet_data = ptr::null();
            let result = next_ex(handle, &mut header, &mut packet_data);
            if result == 0 {
                continue;
            }
            if result < 0 {
                let error = c_string(get_err(handle));
                close(handle);
                return Err(format!("抓包读取失败: {error}"));
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
        close(handle);
    }
    Ok(())
}

pub fn import_pcapng(
    path: PathBuf,
    characters: Arc<HashMap<u32, CharacterInfo>>,
    include_incoming: bool,
    sender: Sender<EngineEvent>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
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
                    None,
                    include_incoming,
                    &characters,
                    &sender,
                );
            }
            if packet_count > 0 && supported_count == 0 {
                return Err("pcapng 中没有受支持的 Ethernet 数据包".to_owned());
            }
            Ok((packet_count, supported_count))
        })();

        let _ = sender.send(EngineEvent::CaptureStopped);
        match result {
            Ok((packet_count, supported_count)) => {
                let _ = sender.send(EngineEvent::Status(format!(
                    "pcapng 导入完成：读取 {packet_count} 包，解析 {supported_count} 个 Ethernet 包"
                )));
            }
            Err(error) => {
                let _ = sender.send(EngineEvent::Error(format!("pcapng 导入失败：{error}")));
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
                    if send_export_packet(
                        packets.next().expect("peeked packet must exist"),
                        &sender,
                    )? {
                        packet_count += 1;
                    }
                } else {
                    sender
                        .send(export_hit_event(
                            hits.next().expect("peeked hit must exist"),
                        ))
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok((hit_count, packet_count))
        })();

        let _ = sender.send(EngineEvent::CaptureStopped);
        match result {
            Ok((hit_count, packet_count)) => {
                let _ = sender.send(EngineEvent::Status(format!(
                    "JSON 导入完成：{packet_count} 个封包，{hit_count} 条伤害"
                )));
            }
            Err(error) => {
                let _ = sender.send(EngineEvent::Error(format!("JSON 导入失败：{error}")));
            }
        }
    })
}

fn send_export_packet(packet: ExportPacket, sender: &Sender<EngineEvent>) -> Result<bool, String> {
    let declared_ids = parse_export_ids(&packet.declared_ids);
    let payload = hex::decode(&packet.payload_hex).unwrap_or_default();
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
    for scene in crate::scene::detect_scenes(packet.timestamp, &packet.decoded_text) {
        sender
            .send(EngineEvent::Scene(scene))
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
                    if line.trim_start().starts_with("\"payload_hex\":") && !line.ends_with(',') {
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
    use std::path::Path;

    fn actual_capture_path() -> PathBuf {
        if let Some(path) = std::env::var_os("NTE_TEST_CAPTURE").map(PathBuf::from) {
            assert!(
                path.is_file(),
                "NTE_TEST_CAPTURE 指向的真实抓包文件不存在: {}",
                path.display()
            );
            return path;
        }

        std::fs::read_dir("logs")
            .expect("logs 目录不存在，无法执行真实数据测试")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("pcapng"))
            })
            .max_by_key(|path| path.metadata().map(|metadata| metadata.len()).unwrap_or(0))
            .expect("logs 目录中没有真实 .pcapng 抓包；也可设置 NTE_TEST_CAPTURE")
    }

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

    fn inferred_target(
        source: MonsterTargetSource,
        observed_at: f64,
        expires_at: f64,
    ) -> InferredMonsterTarget {
        InferredMonsterTarget {
            id: "monster-1".to_owned(),
            name: "测试怪物".to_owned(),
            source,
            observed_at,
            expires_at,
        }
    }

    fn targetless_hit() -> Hit {
        Hit {
            timestamp: 0.0,
            char_id: 1,
            char_name: "测试角色".to_owned(),
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
    fn applies_only_current_unambiguous_inferred_monster() {
        let mut decoder = PacketDecoder::default();
        decoder.current_monster = Some(inferred_target(
            MonsterTargetSource::GameplayEffect,
            10.0,
            13.0,
        ));

        let mut hits = [targetless_hit()];
        decoder.apply_inferred_monster(&mut hits, 12.0, false);
        assert_eq!(hits[0].target_name.as_deref(), Some("测试怪物"));
        assert!(hits[0].target_context[0].contains("推断目标"));
        assert!(hits[0].target_context[0].contains("置信度高"));

        let mut expired = [targetless_hit()];
        decoder.apply_inferred_monster(&mut expired, 13.1, false);
        assert!(expired[0].target_name.is_none());

        let mut ambiguous = [targetless_hit()];
        decoder.apply_inferred_monster(&mut ambiguous, 12.0, true);
        assert!(ambiguous[0].target_name.is_none());
    }

    #[test]
    fn low_confidence_entity_target_has_short_lifetime() {
        let mut decoder = PacketDecoder::default();
        decoder.current_monster = Some(inferred_target(MonsterTargetSource::Entity, 20.0, 20.25));

        let mut near_hits = [targetless_hit()];
        decoder.apply_inferred_monster(&mut near_hits, 20.2, false);
        assert_eq!(near_hits[0].target_name.as_deref(), Some("测试怪物"));
        assert!(near_hits[0].target_context[0].contains("置信度低"));

        let mut later_hits = [targetless_hit()];
        decoder.apply_inferred_monster(&mut later_hits, 20.3, false);
        assert!(later_hits[0].target_name.is_none());
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
            resolve_damage_name("GE_Player_Daffodill_Skill6_Damage", &skills, &names,).as_deref(),
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
            (
                "GE_Test_Skill1_Damage".to_owned(),
                "测试技能短按".to_owned(),
            ),
            (
                "GE_Test_Skill2_Damage".to_owned(),
                "测试技能长按".to_owned(),
            ),
        ]);

        assert_eq!(
            resolve_damage_name("GE_Test_Skill3_Damage", &skills, &names),
            None
        );
    }

    #[test]
    #[ignore = "requires a local PCAPNG via logs or NTE_TEST_CAPTURE"]
    fn actual_capture_contains_udp_traffic() {
        let path = actual_capture_path();
        let file = File::open(&path).expect("无法打开真实抓包文件");
        let mut reader = PcapNgReader::new(file).expect("真实抓包不是有效的 PCAPNG");
        let mut ethernet_packets = 0;
        let mut udp_packets = 0;

        while let Some(block) = reader.next_block() {
            let block = block.expect("真实抓包包含损坏的数据块");
            let (interface_id, data) = match block {
                Block::EnhancedPacket(packet) => {
                    (packet.interface_id as usize, packet.data.into_owned())
                }
                Block::SimplePacket(packet) => (0, packet.data.into_owned()),
                _ => continue,
            };
            let Some(interface) = reader.interfaces().get(interface_id) else {
                continue;
            };
            if interface.linktype != DataLink::ETHERNET {
                continue;
            }
            ethernet_packets += 1;
            if parse_udp_ipv4(&data).is_some() {
                udp_packets += 1;
            }
        }

        assert!(
            ethernet_packets > 0,
            "真实抓包中没有 Ethernet 数据包: {}",
            path.display()
        );
        assert!(
            udp_packets > 0,
            "真实抓包中没有可解析的 IPv4/UDP 流量: {}",
            path.display()
        );
    }

    #[test]
    #[ignore = "requires a local PCAPNG via logs or NTE_TEST_CAPTURE"]
    fn actual_capture_runs_through_the_decoder() {
        let path = actual_capture_path();
        let characters = Arc::new(
            crate::parser::load_characters(Path::new(crate::parser::CHARACTER_DATA_PATH))
                .expect("无法加载实际 characters.json"),
        );
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));

        import_pcapng(path.clone(), characters, true, sender, stop)
            .join()
            .expect("真实抓包导入线程异常退出");

        let events: Vec<_> = receiver.try_iter().collect();
        let packet_count = events
            .iter()
            .filter(|event| matches!(event, EngineEvent::Packet(_)))
            .count();
        let errors: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::Error(error) => Some(error.as_str()),
                _ => None,
            })
            .collect();

        assert!(
            errors.is_empty(),
            "真实抓包导入失败 {}: {}",
            path.display(),
            errors.join("; ")
        );
        assert!(
            packet_count > 0,
            "真实抓包未产生任何解码包事件: {}",
            path.display()
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::CaptureStopped)),
            "真实抓包导入未发送结束事件: {}",
            path.display()
        );
    }

    #[test]
    #[ignore = "manual real-capture scene analysis"]
    fn diagnose_actual_capture_scenes() {
        let path = actual_capture_path();
        let characters = Arc::new(
            crate::parser::load_characters(Path::new(crate::parser::CHARACTER_DATA_PATH))
                .expect("failed to load characters.json"),
        );
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));

        import_pcapng(path.clone(), characters, true, sender, stop)
            .join()
            .expect("capture import thread failed");

        let mut observations = Vec::new();
        let mut accepted_scenes = Vec::new();
        let mut scene_state = crate::model::SceneState::default();
        let mut scene_lines = Vec::new();
        for event in receiver.try_iter() {
            match event {
                EngineEvent::Scene(scene) => {
                    let previous = scene_state.display_name().to_owned();
                    scene_state.apply(scene.clone());
                    let current = scene_state.display_name();
                    if current != previous {
                        accepted_scenes.push((scene.timestamp, current.to_owned()));
                    }
                    if observations.last().is_none_or(
                        |previous: &crate::model::SceneObservation| previous.id != scene.id,
                    ) {
                        observations.push(scene);
                    }
                }
                EngineEvent::Packet(packet) => {
                    for line in packet
                        .decoded_text
                        .lines()
                        .filter(|line| line.contains("DataLayer_") || line.contains("/Map_DLC/"))
                    {
                        scene_lines.push((packet.timestamp, line.to_owned()));
                    }
                }
                _ => {}
            }
        }
        println!("capture={}", path.display());
        for (timestamp, line) in &scene_lines {
            println!("token t={timestamp:.3} {line}");
        }
        for scene in &observations {
            println!(
                "scene t={:.3} id={} name={} category={} priority={}",
                scene.timestamp, scene.id, scene.display_name, scene.category, scene.priority
            );
        }
        for (timestamp, display_name) in &accepted_scenes {
            println!("accepted_scene t={timestamp:.3} name={display_name}");
        }
        assert!(
            !observations.is_empty() || !scene_lines.is_empty(),
            "no scene tokens detected"
        );
    }

    #[test]
    #[ignore = "manual real-capture target analysis"]
    fn diagnose_actual_capture_targets() {
        let path = actual_capture_path();
        let characters = Arc::new(
            crate::parser::load_characters(Path::new(crate::parser::CHARACTER_DATA_PATH))
                .expect("failed to load characters.json"),
        );
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));

        import_pcapng(path.clone(), characters, true, sender, stop)
            .join()
            .expect("capture import thread failed");

        let mut targets = HashMap::<(String, String, String), usize>::new();
        let mut observed_targets = Vec::new();
        let mut unresolved = 0;
        for event in receiver.try_iter() {
            match event {
                EngineEvent::Hit(hit) => match (hit.target_id, hit.target_name) {
                    (Some(id), Some(name)) => {
                        *targets
                            .entry((id, name, hit.target_context.join("|")))
                            .or_default() += 1
                    }
                    _ => unresolved += 1,
                },
                EngineEvent::Packet(packet) => {
                    if let Ok(payload) = hex::decode(&packet.payload_hex) {
                        for (id, name) in detect_monster_targets(&payload) {
                            observed_targets.push((packet.timestamp, id, name));
                        }
                    }
                }
                _ => {}
            }
        }
        observed_targets.dedup();
        println!("observed_targets={observed_targets:?}");
        println!(
            "target_summary capture={} targets={targets:?} unresolved={unresolved}",
            path.display()
        );
        assert!(!targets.is_empty(), "no monster targets resolved");
    }

    #[test]
    #[ignore = "manual real-capture attack type analysis"]
    fn diagnose_actual_capture_attack_types() {
        let path = actual_capture_path();
        let characters = Arc::new(
            crate::parser::load_characters(Path::new(crate::parser::CHARACTER_DATA_PATH))
                .expect("无法加载实际 characters.json"),
        );
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));

        import_pcapng(path.clone(), characters, true, sender, stop)
            .join()
            .expect("真实抓包导入线程异常退出");

        let mut counts = HashMap::<String, usize>::new();
        let mut effects = HashMap::<String, (usize, Vec<u64>)>::new();
        let mut unknown_hits = Vec::new();
        let mut other_hits = Vec::new();
        let mut recent_hits = VecDeque::<Hit>::new();
        let mut recent_packets = VecDeque::<(f64, Vec<u32>)>::new();
        let mut last_declared_at = HashMap::<u32, f64>::new();
        let mut total_hits = 0_usize;
        let mut classified_hits = 0_usize;
        let mut named_hits = 0_usize;
        for event in receiver.try_iter() {
            let hit = match event {
                EngineEvent::Packet(packet) => {
                    if !packet.declared_ids.is_empty() {
                        for character_id in &packet.declared_ids {
                            last_declared_at.insert(*character_id, packet.timestamp);
                        }
                        recent_packets.push_back((packet.timestamp, packet.declared_ids));
                        while recent_packets.len() > 32 {
                            recent_packets.pop_front();
                        }
                    }
                    continue;
                }
                EngineEvent::Hit(hit) => hit,
                _ => continue,
            };
            if hit.attack_type.as_deref() == Some("环合") {
                println!(
                    "qte_timeline t={:.6} char={}({}) damage={:.0} effect={} recent={:?}",
                    hit.timestamp,
                    hit.char_name,
                    hit.char_id,
                    hit.damage,
                    hit.gameplay_effect_name.as_deref().unwrap_or("<unknown>"),
                    recent_hits
                        .iter()
                        .filter(|recent| hit.timestamp - recent.timestamp <= 3.0)
                        .map(|recent| (
                            hit.timestamp - recent.timestamp,
                            recent.char_id,
                            recent.char_name.as_str(),
                            recent.attack_type.as_deref(),
                            recent.gameplay_effect_name.as_deref(),
                        ))
                        .collect::<Vec<_>>()
                );
                println!(
                    "qte_packet_history={:?}",
                    recent_packets
                        .iter()
                        .filter(|(timestamp, _)| hit.timestamp - timestamp <= 1.0)
                        .map(|(timestamp, ids)| (hit.timestamp - timestamp, ids))
                        .collect::<Vec<_>>()
                );
                let mut other_declarations = last_declared_at
                    .iter()
                    .filter(|(character_id, _)| **character_id != hit.char_id)
                    .map(|(character_id, timestamp)| (hit.timestamp - timestamp, *character_id))
                    .collect::<Vec<_>>();
                other_declarations.sort_by(|left, right| left.0.total_cmp(&right.0));
                println!("qte_other_declarations={other_declarations:?}");
            }
            total_hits += 1;
            let attack_type = hit.attack_type.as_deref().unwrap_or("未知").to_owned();
            if hit.attack_type.is_some() {
                classified_hits += 1;
            } else if unknown_hits.len() < 12 {
                unknown_hits.push((
                    hit.char_name.clone(),
                    hit.damage.round() as u64,
                    hit.bit_shift,
                    hit.gameplay_effect_index,
                ));
            }
            if hit.damage_name.is_some() {
                named_hits += 1;
            }
            if hit.attack_type.as_deref() == Some("其他") && other_hits.len() < 12 {
                other_hits.push((
                    hit.char_name.clone(),
                    hit.damage.round() as u64,
                    hit.gameplay_effect_name.clone(),
                    hit.ability_name.clone(),
                ));
            }
            *counts.entry(attack_type).or_default() += 1;
            if let Some(effect_name) = hit.gameplay_effect_name.as_deref() {
                let row = effects.entry(effect_name.to_owned()).or_default();
                row.0 += 1;
                let damage = hit.damage.round() as u64;
                if row.1.len() < 8 && !row.1.contains(&damage) {
                    row.1.push(damage);
                }
            }
            recent_hits.push_back(hit);
            while recent_hits.len() > 24 {
                recent_hits.pop_front();
            }
        }

        let mut count_rows: Vec<_> = counts.into_iter().collect();
        count_rows.sort_by(|left, right| right.1.cmp(&left.1));
        let mut effect_rows: Vec<_> = effects.into_iter().collect();
        effect_rows.sort_by(|left, right| right.1.0.cmp(&left.1.0));
        println!(
            "attack_type_summary capture={} hits={} classified={} named={} types={:?}",
            path.display(),
            total_hits,
            classified_hits,
            named_hits,
            count_rows
        );
        println!("unknown_hits={unknown_hits:?}");
        println!("other_hits={other_hits:?}");
        for (effect_name, (count, damages)) in effect_rows {
            println!("effect={effect_name} hits={count} damage={damages:?}");
        }

        assert!(total_hits > 0, "抓包没有解析出伤害记录");
        assert!(classified_hits > 0, "抓包没有关联出任何攻击类型");
    }

    #[test]
    #[ignore = "requires a local PCAPNG via logs or NTE_TEST_CAPTURE"]
    fn actual_capture_contains_server_boss_hp_updates() {
        let path = actual_capture_path();
        let file = File::open(&path).expect("无法打开真实抓包文件");
        let mut reader = PcapNgReader::new(file).expect("真实抓包不是有效的 PCAPNG");
        let mut updates = Vec::new();

        while let Some(block) = reader.next_block() {
            let block = block.expect("真实抓包包含损坏的数据块");
            let (interface_id, data) = match block {
                Block::EnhancedPacket(packet) => {
                    (packet.interface_id as usize, packet.data.into_owned())
                }
                Block::SimplePacket(packet) => (0, packet.data.into_owned()),
                _ => continue,
            };
            let Some(interface) = reader.interfaces().get(interface_id) else {
                continue;
            };
            if interface.linktype != DataLink::ETHERNET {
                continue;
            }
            let Some((_, _, _, _, payload)) = parse_udp_ipv4(&data) else {
                continue;
            };
            updates.extend(parse_boss_hp_updates(payload));
        }

        assert!(
            updates.len() >= 2,
            "真实抓包中缺少可用于伤害校正的 Boss HP 更新: {}",
            path.display()
        );
        assert!(
            updates
                .windows(2)
                .any(|pair| pair[1].current_hp < pair[0].current_hp),
            "真实抓包中的 Boss HP 更新没有形成掉血序列: {}",
            path.display()
        );
    }

    #[test]
    #[ignore = "manual real-capture GameplayEffect analysis"]
    fn diagnose_actual_capture_gameplay_effects() {
        #[derive(Default)]
        struct EffectStats {
            count: usize,
            packet_count: usize,
            damage_values: Vec<u64>,
            declared_ids: HashSet<u32>,
            bit_shifts: HashSet<u8>,
        }

        let path = actual_capture_path();
        let names = load_gameplay_effect_mapping(Path::new(GAMEPLAY_EFFECT_MAPPING_PATH))
            .expect("无法加载 GameplayEffect 映射");
        let file = File::open(&path).expect("无法打开真实抓包文件");
        let mut reader = PcapNgReader::new(file).expect("真实抓包不是有效的 PCAPNG");
        let mut stats = HashMap::<u32, EffectStats>::new();
        let mut udp_packets = 0_usize;
        let mut effect_packets = 0_usize;

        while let Some(block) = reader.next_block() {
            let block = block.expect("真实抓包包含损坏的数据块");
            let (interface_id, timestamp, data) = match block {
                Block::EnhancedPacket(packet) => (
                    packet.interface_id as usize,
                    packet.timestamp.as_secs_f64(),
                    packet.data.into_owned(),
                ),
                Block::SimplePacket(packet) => (0, 0.0, packet.data.into_owned()),
                _ => continue,
            };
            let Some(interface) = reader.interfaces().get(interface_id) else {
                continue;
            };
            if interface.linktype != DataLink::ETHERNET {
                continue;
            }
            let Some((src, _, dst, _, payload)) = parse_udp_ipv4(&data) else {
                continue;
            };
            udp_packets += 1;
            let effects = parse_gameplay_effects(payload);
            if effects.is_empty() {
                continue;
            }
            effect_packets += 1;
            let damage_records = crate::parser::parse_damage_records(payload);
            let damage_values: Vec<_> = damage_records
                .iter()
                .map(|record| record.damage.round() as u64)
                .collect();
            let character_evidence = find_declared_character_evidence(payload);
            let declared_ids = declared_character_ids_from_evidence(&character_evidence);
            let mut packet_indexes = HashSet::new();
            if effects
                .iter()
                .any(|effect| matches!(effect.unique_index, 52 | 559 | 561))
            {
                println!(
                    "reaction_event t={timestamp:.6} direction={} effects={:?} evidence={:?} damage={:?}",
                    if src.is_private() && !dst.is_private() {
                        "C2S"
                    } else {
                        "S2C"
                    },
                    effects
                        .iter()
                        .map(|effect| (effect.unique_index, effect.bit_shift, effect.byte_offset))
                        .collect::<Vec<_>>(),
                    character_evidence,
                    damage_records
                        .iter()
                        .map(|record| (
                            record.damage.round() as u64,
                            record.bit_shift,
                            record.byte_offset
                        ))
                        .collect::<Vec<_>>()
                );
            }
            for effect in effects {
                let row = stats.entry(effect.unique_index).or_default();
                row.count += 1;
                row.bit_shifts.insert(effect.bit_shift);
                row.declared_ids.extend(declared_ids.iter().copied());
                for damage in &damage_values {
                    if !row.damage_values.contains(damage) && row.damage_values.len() < 12 {
                        row.damage_values.push(*damage);
                    }
                }
                if packet_indexes.insert(effect.unique_index) {
                    row.packet_count += 1;
                }
            }
        }

        let mut rows: Vec<_> = stats.into_iter().collect();
        rows.sort_by(|(left_index, left), (right_index, right)| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left_index.cmp(right_index))
        });
        println!(
            "effect_summary capture={} udp_packets={} effect_packets={} effect_events={} distinct={}",
            path.display(),
            udp_packets,
            effect_packets,
            rows.iter().map(|(_, row)| row.count).sum::<usize>(),
            rows.len()
        );
        for (index, mut row) in rows {
            let mut declared_ids: Vec<_> = row.declared_ids.drain().collect();
            declared_ids.sort_unstable();
            let mut bit_shifts: Vec<_> = row.bit_shifts.drain().collect();
            bit_shifts.sort_unstable();
            println!(
                "effect index={} count={} packets={} name={} declared={:?} damage={:?} shifts={:?}",
                index,
                row.count,
                row.packet_count,
                names.get(&index).map(String::as_str).unwrap_or("<unknown>"),
                declared_ids,
                row.damage_values,
                bit_shifts
            );
        }
    }

    #[test]
    #[ignore = "manual real-capture follow-up damage analysis"]
    fn diagnose_actual_capture_follow_up_damage() {
        let path = actual_capture_path();
        let characters = Arc::new(
            crate::parser::load_characters(Path::new(crate::parser::CHARACTER_DATA_PATH))
                .expect("无法加载实际 characters.json"),
        );
        let (sender, receiver) = unbounded();
        import_pcapng(
            path.clone(),
            characters,
            true,
            sender,
            Arc::new(AtomicBool::new(false)),
        )
        .join()
        .unwrap();

        let mut hits: Vec<_> = receiver
            .try_iter()
            .filter_map(|event| match event {
                EngineEvent::Hit(hit) if hit.direction != "incoming" => Some(hit),
                _ => None,
            })
            .collect();
        hits.sort_by(|left, right| left.timestamp.total_cmp(&right.timestamp));
        let max_hp = hits
            .iter()
            .map(|hit| hit.target_max_hp.round() as u64)
            .max()
            .unwrap();
        hits.retain(|hit| hit.target_max_hp.round() as u64 == max_hp);
        for hit in &hits {
            println!(
                "parsed_hit t={:.6} char={} damage={:.3} hp_before={:.3} hp_after={:.3} max_hp={:.3} source={} target={:?}",
                hit.timestamp,
                hit.char_id,
                hit.damage,
                hit.target_hp_before,
                hit.target_hp_after,
                hit.target_max_hp,
                hit.char_source,
                hit.target_id
            );
        }

        let mut gaps = Vec::new();
        for pair in hits.windows(2) {
            let gap = pair[0].target_hp_after - pair[1].target_hp_before;
            if gap > 0.5 {
                gaps.push((pair[1].timestamp, gap));
            }
        }
        println!(
            "capture={} max_hp={} hits={} damage={:.0} first_hp={:.0} last_hp={:.0} gaps={} gap_sum={:.0}",
            path.display(),
            max_hp,
            hits.len(),
            hits.iter().map(|hit| hit.damage).sum::<f64>(),
            hits.first().unwrap().target_hp_before,
            hits.last().unwrap().target_hp_after,
            gaps.len(),
            gaps.iter().map(|(_, gap)| gap).sum::<f64>()
        );
        let start = hits.first().unwrap().timestamp;
        let mut frequencies: HashMap<u64, Vec<f64>> = HashMap::new();
        for (timestamp, gap) in &gaps {
            frequencies
                .entry(gap.round() as u64)
                .or_default()
                .push(*timestamp);
        }
        let mut frequencies: Vec<_> = frequencies.into_iter().collect();
        frequencies.sort_by_key(|(_, timestamps)| std::cmp::Reverse(timestamps.len()));
        for (damage, timestamps) in frequencies.iter().take(30) {
            let intervals: Vec<_> = timestamps
                .windows(2)
                .map(|pair| pair[1] - pair[0])
                .collect();
            println!(
                "gap_damage={} count={} first={:.3}s intervals={:?}",
                damage,
                timestamps.len(),
                timestamps[0] - start,
                intervals
                    .iter()
                    .map(|interval| format!("{interval:.3}"))
                    .collect::<Vec<_>>()
            );
        }

        #[derive(Clone)]
        struct RawPacket {
            timestamp: f64,
            direction: &'static str,
            payload: Vec<u8>,
            hit_count: usize,
            text: String,
        }
        let file = File::open(&path).unwrap();
        let mut reader = PcapNgReader::new(file).unwrap();
        let mut packets = Vec::new();
        while let Some(block) = reader.next_block() {
            let block = block.unwrap();
            let (interface_id, timestamp, data) = match block {
                Block::EnhancedPacket(packet) => (
                    packet.interface_id as usize,
                    packet.timestamp.as_secs_f64(),
                    packet.data.into_owned(),
                ),
                Block::SimplePacket(packet) => (0, 0.0, packet.data.into_owned()),
                _ => continue,
            };
            let Some(interface) = reader.interfaces().get(interface_id) else {
                continue;
            };
            if interface.linktype != DataLink::ETHERNET {
                continue;
            }
            let Some((src, _, dst, _, payload)) = parse_udp_ipv4(&data) else {
                continue;
            };
            for record in crate::parser::parse_damage_records(payload) {
                println!(
                    "raw_record t={timestamp:.6} dir={} damage={:.6} hp_before={:.6} max_hp={:.6} damage_time={:.6} world_time={:.6} repeated={:.6} flags={:?} trailing={:.6} shift={} offset={}",
                    if src.is_private() && !dst.is_private() {
                        "C2S"
                    } else {
                        "S2C"
                    },
                    record.damage,
                    record.target_hp_before,
                    record.target_max_hp,
                    record.damage_time,
                    record.world_time,
                    record.repeated_damage,
                    record.state_flags,
                    record.trailing_value,
                    record.bit_shift,
                    record.byte_offset
                );
            }
            let search_values = [641.0_f32, 3_177.0, 3_209.0, 3_818.0, 3_850.0, 8_772.0];
            for bit_shift in 0..8 {
                let shifted = decode_shifted_payload(payload, bit_shift);
                for offset in 0..shifted.len().saturating_sub(4) {
                    let value = f32::from_le_bytes(shifted[offset..offset + 4].try_into().unwrap());
                    for expected in search_values {
                        if value.is_finite() && (value - expected).abs() < 0.01 {
                            println!(
                                "float_value t={timestamp:.6} shift={bit_shift} offset={offset} value={value:.3}"
                            );
                        }
                    }
                    if value.is_finite()
                        && ((0.15..=0.25).contains(&value) || (0.95..=1.05).contains(&value))
                    {
                        println!(
                            "ratio_value t={timestamp:.6} shift={bit_shift} offset={offset} value={value:.9}"
                        );
                    }
                    if !src.is_private()
                        && dst.is_private()
                        && timestamp >= 1_781_344_072.474
                        && value.is_finite()
                        && (1_100_000.0..=1_140_000.0).contains(&value)
                        && value.fract().abs() < 0.01
                    {
                        println!(
                            "boss_hp_candidate t={timestamp:.6} shift={bit_shift} offset={offset} value={value:.0} len={}",
                            payload.len()
                        );
                        if (value - 1_122_225.0).abs() < 0.5 || (value - 1_126_075.0).abs() < 0.5 {
                            let start = offset.saturating_sub(40);
                            let end = (offset + 40).min(shifted.len());
                            println!(
                                "boss_hp_context text={} bytes={}",
                                decode_payload_text(payload).replace('\n', "|"),
                                hex::encode(&shifted[start..end])
                            );
                        }
                    }
                }
            }
            packets.push(RawPacket {
                timestamp,
                direction: if src.is_private() && !dst.is_private() {
                    "C2S"
                } else {
                    "S2C"
                },
                payload: payload.to_vec(),
                hit_count: crate::parser::parse_damage_records(payload).len(),
                text: decode_payload_text(payload),
            });
        }

        let Some(trigger_time) = hits
            .iter()
            .find(|hit| (hit.damage - 8_772.0).abs() < 0.5)
            .map(|hit| hit.timestamp)
        else {
            println!("specific trigger damage 8772 is absent; skipping legacy packet window");
            return;
        };
        let Some(follow_up_time) = hits
            .iter()
            .find(|hit| (hit.damage - 3_177.0).abs() < 0.5)
            .map(|hit| hit.timestamp)
        else {
            println!("specific follow-up damage 3177 is absent; skipping legacy packet window");
            return;
        };
        for (label, center) in [("trigger", trigger_time), ("follow_up", follow_up_time)] {
            println!("window {label} center={center:.6}");
            for packet in packets.iter().filter(|packet| {
                packet.direction == "C2S"
                    && packet.timestamp >= center - 0.5
                    && packet.timestamp <= center + 0.5
            }) {
                let evidence = find_declared_character_evidence(&packet.payload);
                println!(
                    "window_packet label={label} dt={:.6} len={} hits={} ids={:?} text={} prefix={}",
                    packet.timestamp - center,
                    packet.payload.len(),
                    packet.hit_count,
                    declared_character_ids_from_evidence(&evidence),
                    packet.text.replace('\n', "|"),
                    hex::encode(&packet.payload[..packet.payload.len().min(48)])
                );
            }
        }
        for packet in &packets {
            if packet.text.lines().any(|line| {
                line.contains("GA_")
                    || line.starts_with("Ability.")
                    || line.starts_with("Effect")
                    || line.contains("ActiveGE")
            }) {
                println!(
                    "effect_text dt_trigger={:.6} dt_follow_up={:.6} dir={} len={} text={}",
                    packet.timestamp - trigger_time,
                    packet.timestamp - follow_up_time,
                    packet.direction,
                    packet.payload.len(),
                    packet.text.replace('\n', "|")
                );
            }
        }
        for target in [641_u32, 3_177, 3_209, 3_850, 8_772] {
            let u16_le = (target as u16).to_le_bytes();
            let u16_be = (target as u16).to_be_bytes();
            let u32_le = target.to_le_bytes();
            let u32_be = target.to_be_bytes();
            let f64_le = (target as f64).to_le_bytes();
            let mut leb128 = Vec::new();
            let mut remaining = target;
            loop {
                let mut byte = (remaining & 0x7f) as u8;
                remaining >>= 7;
                if remaining != 0 {
                    byte |= 0x80;
                }
                leb128.push(byte);
                if remaining == 0 {
                    break;
                }
            }
            for packet in &packets {
                if (packet.timestamp - follow_up_time).abs() > 0.2 {
                    continue;
                }
                for bit_shift in 0..8 {
                    let shifted = decode_shifted_payload(&packet.payload, bit_shift);
                    for (encoding, needle) in [
                        ("u16_le", u16_le.as_slice()),
                        ("u16_be", u16_be.as_slice()),
                        ("u32_le", u32_le.as_slice()),
                        ("u32_be", u32_be.as_slice()),
                        ("f64_le", f64_le.as_slice()),
                        ("leb128", leb128.as_slice()),
                    ] {
                        for offset in shifted
                            .windows(needle.len())
                            .enumerate()
                            .filter_map(|(offset, window)| (window == needle).then_some(offset))
                        {
                            println!(
                                "encoded_value target={target} encoding={encoding} dt={:.6} dir={} len={} shift={bit_shift} offset={offset} context={}",
                                packet.timestamp - follow_up_time,
                                packet.direction,
                                packet.payload.len(),
                                hex::encode(
                                    &shifted[offset.saturating_sub(12)
                                        ..(offset + needle.len() + 12).min(shifted.len())]
                                )
                            );
                        }
                    }
                }
            }
        }
        for target in [0.2_f32, 1.2, 20.0] {
            let needle = target.to_le_bytes();
            for packet in &packets {
                if (packet.timestamp - follow_up_time).abs() > 0.2 {
                    continue;
                }
                for bit_shift in 0..8 {
                    let shifted = decode_shifted_payload(&packet.payload, bit_shift);
                    for offset in shifted
                        .windows(needle.len())
                        .enumerate()
                        .filter_map(|(offset, window)| (window == needle).then_some(offset))
                    {
                        println!(
                            "coefficient target={target} dt={:.6} dir={} len={} shift={bit_shift} offset={offset}",
                            packet.timestamp - follow_up_time,
                            packet.direction,
                            packet.payload.len()
                        );
                    }
                }
            }
        }
        for (label, center) in [("trigger", trigger_time), ("follow_up", follow_up_time)] {
            let packet = packets
                .iter()
                .find(|packet| packet.timestamp == center && packet.hit_count > 0)
                .unwrap();
            for bit_shift in 0..8 {
                let shifted = decode_shifted_payload(&packet.payload, bit_shift);
                let strings = shifted
                    .split(|byte| !(0x20..=0x7e).contains(byte))
                    .filter(|bytes| bytes.len() >= 4)
                    .filter_map(|bytes| std::str::from_utf8(bytes).ok())
                    .collect::<Vec<_>>();
                if !strings.is_empty() {
                    println!("hit_strings label={label} shift={bit_shift} values={strings:?}");
                }
                if let Some(offset) = shifted
                    .windows(b"FHTClientActiveGE".len())
                    .position(|window| window == b"FHTClientActiveGE")
                {
                    let start = offset.saturating_sub(96);
                    let end = (offset + 256).min(shifted.len());
                    println!(
                        "active_ge label={label} shift={bit_shift} offset={offset} nearby={}",
                        hex::encode(&shifted[start..end])
                    );
                    let anchor = offset + b"FHTClientActiveGE".len() + 5;
                    let values = shifted[anchor..]
                        .chunks_exact(4)
                        .enumerate()
                        .filter_map(|(index, bytes)| {
                            let value = u32::from_le_bytes(bytes.try_into().unwrap());
                            (2..=10_000_000)
                                .contains(&value)
                                .then_some((index * 4, value))
                        })
                        .collect::<Vec<_>>();
                    println!("active_ge_u32 label={label} values={values:?}");
                }
            }
            for record in crate::parser::parse_damage_records(&packet.payload) {
                let shifted = decode_shifted_payload(&packet.payload, record.bit_shift);
                let start = record.byte_offset.saturating_sub(96);
                let end = (record.byte_offset + 128).min(shifted.len());
                println!(
                    "hit_record label={label} damage={} shift={} offset={} nearby={}",
                    record.damage,
                    record.bit_shift,
                    record.byte_offset,
                    hex::encode(&shifted[start..end])
                );
            }
        }

        let mut signature_hits: HashMap<(String, usize, String), usize> = HashMap::new();
        let mut exact_float_matches = 0;
        for pair in hits.windows(2) {
            let gap = pair[0].target_hp_after - pair[1].target_hp_before;
            if gap <= 0.5 {
                continue;
            }
            let float_bytes = (gap as f32).to_le_bytes();
            for packet in packets.iter().filter(|packet| {
                packet.timestamp >= pair[0].timestamp
                    && packet.timestamp <= pair[1].timestamp
                    && packet.hit_count == 0
            }) {
                if packet
                    .payload
                    .windows(float_bytes.len())
                    .any(|window| window == float_bytes)
                {
                    exact_float_matches += 1;
                    println!(
                        "float_match gap={gap:.0} dt={:.3} dir={} len={} text={}",
                        packet.timestamp - pair[0].timestamp,
                        packet.direction,
                        packet.payload.len(),
                        packet.text.replace('\n', "|")
                    );
                }
                let text = if packet.text == UNREADABLE_PROTOCOL_TEXT {
                    String::new()
                } else {
                    packet.text.lines().take(3).collect::<Vec<_>>().join("|")
                };
                *signature_hits
                    .entry((packet.direction.to_owned(), packet.payload.len(), text))
                    .or_default() += 1;
            }
        }
        println!("exact_float_matches={exact_float_matches}");
        let mut signature_hits: Vec<_> = signature_hits.into_iter().collect();
        signature_hits.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for ((direction, len, text), count) in signature_hits.into_iter().take(60) {
            println!("candidate count={count} dir={direction} len={len} text={text}");
        }

        let mut records_by_direction: HashMap<String, (usize, f64)> = HashMap::new();
        for packet in &packets {
            let records = crate::parser::parse_damage_records(&packet.payload);
            let entry = records_by_direction
                .entry(packet.direction.to_owned())
                .or_default();
            entry.0 += records.len();
            entry.1 += records
                .iter()
                .map(|record| record.damage as f64)
                .sum::<f64>();
        }
        for (direction, (count, damage)) in records_by_direction {
            println!("records direction={direction} count={count} damage={damage:.0}");
        }

        let mut packet_groups: Vec<Vec<&Hit>> = Vec::new();
        for hit in &hits {
            if packet_groups
                .last()
                .is_none_or(|group| group[0].timestamp != hit.timestamp)
            {
                packet_groups.push(Vec::new());
            }
            packet_groups.last_mut().unwrap().push(hit);
        }
        let mut residuals = Vec::new();
        for pair in packet_groups.windows(2) {
            let previous = &pair[0];
            let current = &pair[1];
            let previous_before = previous
                .iter()
                .map(|hit| hit.target_hp_before)
                .fold(f64::NEG_INFINITY, f64::max);
            let previous_damage = previous.iter().map(|hit| hit.damage).sum::<f64>();
            let previous_after = (previous_before - previous_damage).max(0.0);
            let current_before = current
                .iter()
                .map(|hit| hit.target_hp_before)
                .fold(f64::NEG_INFINITY, f64::max);
            residuals.push((
                current[0].timestamp - start,
                previous_after - current_before,
                previous.len(),
                current.len(),
            ));
        }
        let positive = residuals
            .iter()
            .filter(|(_, residual, _, _)| *residual > 0.5)
            .map(|(_, residual, _, _)| *residual)
            .sum::<f64>();
        let negative = residuals
            .iter()
            .filter(|(_, residual, _, _)| *residual < -0.5)
            .map(|(_, residual, _, _)| -*residual)
            .sum::<f64>();
        println!(
            "packet_groups={} positive_residual={positive:.0} negative_residual={negative:.0} net={:.0}",
            packet_groups.len(),
            positive - negative
        );
        for (elapsed, residual, previous_count, current_count) in residuals
            .iter()
            .filter(|(_, residual, _, _)| residual.abs() > 100.0)
        {
            println!(
                "packet_residual t={elapsed:.3}s value={residual:.0} prev_hits={previous_count} current_hits={current_count}"
            );
        }

        let mut positive_counts: HashMap<i64, usize> = HashMap::new();
        let mut negative_counts: HashMap<i64, usize> = HashMap::new();
        for (_, residual, _, _) in &residuals {
            let rounded = residual.round() as i64;
            if rounded > 0 {
                *positive_counts.entry(rounded).or_default() += 1;
            } else if rounded < 0 {
                *negative_counts.entry(-rounded).or_default() += 1;
            }
        }
        let mut unmatched = Vec::new();
        for (damage, positive_count) in positive_counts {
            let negative_count = negative_counts.get(&damage).copied().unwrap_or(0);
            if positive_count > negative_count {
                unmatched.push((damage, positive_count - negative_count));
            }
        }
        unmatched.sort_by_key(|(damage, count)| (std::cmp::Reverse(*count), *damage));
        println!(
            "unmatched_positive_total={}",
            unmatched
                .iter()
                .map(|(damage, count)| *damage * *count as i64)
                .sum::<i64>()
        );
        for (damage, count) in unmatched {
            println!("unmatched damage={damage} count={count}");
        }

        let mut hp_order = hits.iter().collect::<Vec<_>>();
        hp_order.sort_by(|left, right| {
            right
                .target_hp_before
                .total_cmp(&left.target_hp_before)
                .then_with(|| left.timestamp.total_cmp(&right.timestamp))
        });
        let mut hp_gaps = Vec::new();
        let mut overlaps = Vec::new();
        for pair in hp_order.windows(2) {
            let residual = pair[0].target_hp_after - pair[1].target_hp_before;
            if residual > 0.5 {
                hp_gaps.push((
                    pair[1].timestamp - start,
                    residual,
                    pair[0].target_hp_after,
                    pair[1].target_hp_before,
                ));
            } else if residual < -0.5 {
                overlaps.push(-residual);
            }
        }
        println!(
            "hp_order gaps={} gap_sum={:.0} overlaps={} overlap_sum={:.0}",
            hp_gaps.len(),
            hp_gaps.iter().map(|(_, gap, _, _)| gap).sum::<f64>(),
            overlaps.len(),
            overlaps.iter().sum::<f64>()
        );
        let mut hp_gap_frequencies: HashMap<u64, usize> = HashMap::new();
        for (_, gap, _, _) in &hp_gaps {
            *hp_gap_frequencies.entry(gap.round() as u64).or_default() += 1;
        }
        let mut hp_gap_frequencies: Vec<_> = hp_gap_frequencies.into_iter().collect();
        hp_gap_frequencies.sort_by_key(|(damage, count)| (std::cmp::Reverse(*count), *damage));
        for (damage, count) in hp_gap_frequencies {
            println!("hp_order_gap damage={damage} count={count}");
        }

        let mut text_counts: HashMap<String, usize> = HashMap::new();
        for packet in &packets {
            if packet.text == UNREADABLE_PROTOCOL_TEXT {
                continue;
            }
            for line in packet.text.lines() {
                *text_counts.entry(line.to_owned()).or_default() += 1;
            }
        }
        let mut text_counts: Vec<_> = text_counts.into_iter().collect();
        text_counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for (text, count) in text_counts {
            println!("protocol_text count={count} value={text}");
        }

        let mut hits_by_character: HashMap<u32, Vec<usize>> = HashMap::new();
        for (index, hit) in hits.iter().enumerate() {
            hits_by_character
                .entry(hit.char_id)
                .or_default()
                .push(index);
        }
        for (char_id, indexes) in hits_by_character {
            let mut positive = 0.0;
            let mut negative = 0.0;
            let mut residual_values = Vec::new();
            let mut cumulative = 0.0;
            for pair in indexes.windows(2) {
                let previous_index = pair[0];
                let current_index = pair[1];
                let ordinary_between = hits[previous_index..current_index]
                    .iter()
                    .map(|hit| hit.damage)
                    .sum::<f64>();
                let residual = hits[previous_index].target_hp_before
                    - ordinary_between
                    - hits[current_index].target_hp_before;
                cumulative += residual;
                println!(
                    "character_stream_cumulative char_id={char_id} t={:.3}s delta={residual:.0} total={cumulative:.0}",
                    hits[current_index].timestamp - start
                );
                if residual > 0.5 {
                    positive += residual;
                    residual_values.push((hits[current_index].timestamp - start, residual));
                } else if residual < -0.5 {
                    negative += -residual;
                }
            }
            println!(
                "character_stream char_id={char_id} records={} positive={positive:.0} negative={negative:.0} net={:.0}",
                indexes.len(),
                positive - negative
            );
            for (elapsed, residual) in residual_values {
                println!(
                    "character_stream_gap char_id={char_id} t={elapsed:.3}s damage={residual:.0}"
                );
            }
        }

        let mut active_ge_signatures: HashMap<(String, usize, usize, String), usize> =
            HashMap::new();
        for packet in &packets {
            if !packet.text.contains("FHTClientActiveGE") {
                continue;
            }
            let identifiers = packet
                .text
                .lines()
                .filter(|line| {
                    *line != "FHTClientActiveGE"
                        && *line != "FCharacterForNet"
                        && *line != "AbilitySystemComponent"
                        && *line != "UnbalCurrent"
                })
                .collect::<Vec<_>>()
                .join("|");
            *active_ge_signatures
                .entry((
                    packet.direction.to_owned(),
                    packet.payload.len(),
                    packet.hit_count,
                    identifiers,
                ))
                .or_default() += 1;
        }
        let mut active_ge_signatures: Vec<_> = active_ge_signatures.into_iter().collect();
        active_ge_signatures.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for ((direction, len, hit_count, identifiers), count) in active_ge_signatures {
            println!(
                "active_ge count={count} dir={direction} len={len} hits={hit_count} ids={identifiers}"
            );
        }
        for packet in packets
            .iter()
            .filter(|packet| packet.hit_count == 0 && packet.text.contains("FHTClientActiveGE"))
        {
            let evidence = find_declared_character_evidence(&packet.payload);
            println!(
                "active_ge_no_hit t={:.3}s dir={} len={} declared={:?} evidence={:?} text={} hex={}",
                packet.timestamp - start,
                packet.direction,
                packet.payload.len(),
                declared_character_ids_from_evidence(&evidence),
                evidence,
                packet.text.replace('\n', "|"),
                hex::encode(&packet.payload)
            );
            for record in crate::parser::parse_damage_records(&packet.payload) {
                println!(
                    "active_ge_no_hit_record t={:.3}s damage={:.0} hp_before={:.0} max_hp={:.0} repeated={:.0} flags={:?} trailing={:.3} shift={} offset={}",
                    packet.timestamp - start,
                    record.damage,
                    record.target_hp_before,
                    record.target_max_hp,
                    record.repeated_damage,
                    record.state_flags,
                    record.trailing_value,
                    record.bit_shift,
                    record.byte_offset
                );
            }
            for bit_shift in 0..8 {
                let shifted = decode_shifted_payload(&packet.payload, bit_shift);
                for offset in 0..shifted.len().saturating_sub(9) {
                    if shifted[offset] != 12 || shifted[offset + 1..offset + 5] != [4, 0, 0, 0] {
                        continue;
                    }
                    let value =
                        f32::from_le_bytes(shifted[offset + 5..offset + 9].try_into().unwrap());
                    if !value.is_finite() || value.abs() < 1.0 {
                        continue;
                    }
                    let mut cursor = offset;
                    let mut fields = Vec::new();
                    while cursor + 5 <= shifted.len() && fields.len() < 16 {
                        let field_type = shifted[cursor];
                        let length =
                            u32::from_le_bytes(shifted[cursor + 1..cursor + 5].try_into().unwrap())
                                as usize;
                        if length == 0 || length > 64 || cursor + 5 + length > shifted.len() {
                            break;
                        }
                        fields.push(format!("{field_type}:{length}"));
                        cursor += 5 + length;
                    }
                    if fields.len() >= 3 {
                        println!(
                            "active_ge_fields t={:.3}s shift={} offset={} first={value:.3} fields={}",
                            packet.timestamp - start,
                            bit_shift,
                            offset,
                            fields.join(",")
                        );
                    }
                }
            }
        }

        #[derive(Clone, Copy)]
        struct StreamEstimate {
            hp_before: f64,
            ordinary_total: f64,
            inferred_total: f64,
        }
        let mut stream_estimates: HashMap<u32, StreamEstimate> = HashMap::new();
        let mut ordinary_total = 0.0;
        let mut confirmed = 0.0_f64;
        let mut confirmed_events = Vec::new();
        for hit in &hits {
            if let Some(previous) = stream_estimates.get(&hit.char_id).copied() {
                let expected_hp = previous.hp_before - (ordinary_total - previous.ordinary_total);
                let residual = expected_hp - hit.target_hp_before;
                stream_estimates.insert(
                    hit.char_id,
                    StreamEstimate {
                        hp_before: hit.target_hp_before,
                        ordinary_total,
                        inferred_total: previous.inferred_total + residual,
                    },
                );
            } else {
                stream_estimates.insert(
                    hit.char_id,
                    StreamEstimate {
                        hp_before: hit.target_hp_before,
                        ordinary_total,
                        inferred_total: 0.0,
                    },
                );
            }
            if stream_estimates.len() >= 2 {
                let common = stream_estimates
                    .values()
                    .map(|estimate| estimate.inferred_total)
                    .fold(f64::INFINITY, f64::min)
                    .max(0.0);
                if common > confirmed + 0.5 {
                    confirmed_events.push((hit.timestamp - start, common - confirmed));
                    confirmed = common;
                }
            }
            ordinary_total += hit.damage;
        }
        println!(
            "confirmed_burn total={confirmed:.0} events={}",
            confirmed_events.len()
        );
        for (elapsed, damage) in confirmed_events {
            println!("confirmed_burn_event t={elapsed:.3}s damage={damage:.0}");
        }
    }

    #[test]
    #[ignore = "manual real-capture follow-up settlement timeline"]
    fn diagnose_actual_capture_follow_up_settlements() {
        let path = actual_capture_path();
        let characters = Arc::new(
            crate::parser::load_characters(Path::new(crate::parser::CHARACTER_DATA_PATH))
                .expect("无法加载实际 characters.json"),
        );
        let (sender, receiver) = unbounded();
        import_pcapng(
            path.clone(),
            characters.clone(),
            true,
            sender,
            Arc::new(AtomicBool::new(false)),
        )
        .join()
        .unwrap();
        let mut production_hits = Vec::new();
        for event in receiver.try_iter() {
            match event {
                EngineEvent::Hit(hit) => {
                    production_hits.push(hit.clone());
                    println!(
                        "production_hit t={:.6} char={} damage={:.0} direction={} source={}",
                        hit.timestamp, hit.char_id, hit.damage, hit.direction, hit.char_source
                    );
                    if hit.char_source == "boss_hp_residual" {
                        println!(
                            "production_follow_up t={:.6} damage={:.0} context={}",
                            hit.timestamp,
                            hit.damage,
                            hit.target_context.join("|")
                        );
                    }
                }
                EngineEvent::Packet(packet) if packet.note.contains("Boss HP 更新") => {
                    println!(
                        "production_boss_hp t={:.6} source={} destination={} direction={} note={}",
                        packet.timestamp,
                        packet.source,
                        packet.destination,
                        packet.direction,
                        packet.note.replace('\n', "|")
                    );
                }
                _ => {}
            }
        }
        let file = File::open(&path).expect("无法打开真实抓包文件");
        let mut reader = PcapNgReader::new(file).expect("真实抓包不是有效的 PCAPNG");
        let mut timeline = Vec::<(f64, Vec<Hit>, Vec<f64>)>::new();

        while let Some(block) = reader.next_block() {
            let block = block.expect("真实抓包包含损坏的数据块");
            let (interface_id, timestamp, data) = match block {
                Block::EnhancedPacket(packet) => (
                    packet.interface_id as usize,
                    packet.timestamp.as_secs_f64(),
                    packet.data.into_owned(),
                ),
                Block::SimplePacket(packet) => (0, 0.0, packet.data.into_owned()),
                _ => continue,
            };
            let Some(interface) = reader.interfaces().get(interface_id) else {
                continue;
            };
            if interface.linktype != DataLink::ETHERNET {
                continue;
            }
            let Some((src, src_port, dst, dst_port, payload)) = parse_udp_ipv4(&data) else {
                continue;
            };
            for hit in production_hits
                .iter()
                .take(30)
                .filter(|hit| timestamp >= hit.timestamp && timestamp <= hit.timestamp + 0.35)
            {
                let expected_hp = hit.target_hp_after as f32;
                for bit_shift in 0..8 {
                    let shifted = decode_shifted_payload(payload, bit_shift);
                    for offset in shifted
                        .windows(4)
                        .enumerate()
                        .filter_map(|(offset, bytes)| {
                            let value = f32::from_le_bytes(bytes.try_into().unwrap());
                            ((value - expected_hp).abs() < 0.5).then_some(offset)
                        })
                    {
                        println!(
                            "hp_reverse_match hit_t={:.6} packet_t={timestamp:.6} damage={:.0} expected_hp={expected_hp:.0} src={src}:{src_port} dst={dst}:{dst_port} shift={bit_shift} offset={offset} len={} context={}",
                            hit.timestamp,
                            hit.damage,
                            payload.len(),
                            hex::encode(
                                &shifted
                                    [offset.saturating_sub(48)..(offset + 48).min(shifted.len())]
                            )
                        );
                    }
                }
            }
            let evidence = find_declared_character_evidence(payload);
            let ids = declared_character_ids_from_evidence(&evidence);
            let outgoing = src.is_private() && !dst.is_private();
            let hits = if outgoing {
                parse_damage_payload(
                    payload,
                    timestamp,
                    (ids.len() == 1).then(|| ids[0]),
                    None,
                    &characters,
                    &evidence,
                )
            } else {
                Vec::new()
            };
            if outgoing {
                for record in crate::parser::parse_damage_records(payload) {
                    println!(
                        "game_clock packet={timestamp:.6} damage={:.0} damage_time={:.6} world_time={:.6}",
                        record.damage, record.damage_time, record.world_time
                    );
                }
            }
            let hp_updates = if outgoing {
                Vec::new()
            } else {
                parse_boss_hp_updates(payload)
                    .into_iter()
                    .map(|update| update.current_hp as f64)
                    .collect()
            };
            if !hits.is_empty() || !hp_updates.is_empty() {
                let _ = (src_port, dst_port);
                timeline.push((timestamp, hits, hp_updates));
            }
        }

        let mut replay_pending = Vec::<Hit>::new();
        let mut replay_last_hp = None::<f64>;
        let mut accepted_residual = 0.0;
        let mut rejected_positive = Vec::new();
        let mut unmatched_drops = Vec::new();
        for (timestamp, hits, hp_updates) in &timeline {
            for hit in hits {
                if replay_pending
                    .last()
                    .is_some_and(|previous| hit.timestamp - previous.timestamp > 1.0)
                {
                    replay_pending.clear();
                }
                replay_pending.push(hit.clone());
            }
            for current_hp in hp_updates {
                replay_pending.retain(|hit| timestamp - hit.timestamp <= 1.0);
                let previous_hp = replay_last_hp
                    .or_else(|| replay_pending.first().map(|hit| hit.target_hp_before));
                replay_last_hp = Some(*current_hp);
                let Some(previous_hp) = previous_hp else {
                    continue;
                };
                if *current_hp >= previous_hp || replay_pending.is_empty() {
                    if *current_hp > previous_hp {
                        replay_pending.clear();
                    } else if *current_hp < previous_hp {
                        unmatched_drops.push((
                            *timestamp,
                            previous_hp - current_hp,
                            replay_pending
                                .iter()
                                .map(|hit| (hit.timestamp, hit.char_id, hit.damage))
                                .collect::<Vec<_>>(),
                        ));
                    }
                    continue;
                }
                let actual = previous_hp - current_hp;
                let candidate = replay_pending
                    .iter()
                    .enumerate()
                    .filter_map(|(index, hit)| {
                        let ratio = actual / hit.damage;
                        (0.75..=1.35).contains(&ratio).then_some((
                            index,
                            (actual - hit.damage).abs() / actual.max(hit.damage),
                            hit.timestamp,
                        ))
                    })
                    .min_by(|left, right| {
                        left.1
                            .total_cmp(&right.1)
                            .then_with(|| right.2.total_cmp(&left.2))
                    });
                let Some((index, _, _)) = candidate else {
                    unmatched_drops.push((
                        *timestamp,
                        actual,
                        replay_pending
                            .iter()
                            .map(|hit| (hit.timestamp, hit.char_id, hit.damage))
                            .collect::<Vec<_>>(),
                    ));
                    continue;
                };
                let hit = replay_pending.remove(index);
                let residual = actual - hit.damage;
                let ratio = residual / hit.damage;
                if residual >= 1.0 && (0.18..=0.26).contains(&ratio) {
                    accepted_residual += residual;
                } else if residual > 0.5 {
                    rejected_positive.push((
                        *timestamp,
                        actual,
                        hit.damage,
                        residual,
                        ratio,
                        replay_pending
                            .iter()
                            .map(|candidate| {
                                (candidate.timestamp, candidate.char_id, candidate.damage)
                            })
                            .collect::<Vec<_>>(),
                    ));
                }
            }
        }
        rejected_positive.sort_by(|left, right| right.3.total_cmp(&left.3));
        unmatched_drops.sort_by(|left, right| right.1.total_cmp(&left.1));
        println!(
            "replay_summary accepted={accepted_residual:.0} rejected_positive={:.0} unmatched_drops={:.0} pending_damage={:.0}",
            rejected_positive.iter().map(|row| row.3).sum::<f64>(),
            unmatched_drops.iter().map(|row| row.1).sum::<f64>(),
            replay_pending.iter().map(|hit| hit.damage).sum::<f64>()
        );
        for (timestamp, actual, reported, residual, ratio, remaining) in
            rejected_positive.iter().take(30)
        {
            println!(
                "replay_rejected t={timestamp:.6} actual={actual:.0} reported={reported:.0} residual={residual:.0} ratio={:.1}% remaining={remaining:?}",
                ratio * 100.0,
            );
        }
        for (timestamp, actual, pending) in unmatched_drops.iter().take(30) {
            println!("replay_unmatched t={timestamp:.6} actual={actual:.0} pending={pending:?}");
        }

        let mut fifo_pending = std::collections::VecDeque::<Hit>::new();
        let mut fifo_last_hp = None::<f64>;
        let mut fifo_positive_residual = 0.0;
        let mut fifo_overlay_residual = 0.0;
        let mut fifo_negative_residual = 0.0;
        for (timestamp, hits, hp_updates) in &timeline {
            for hit in hits {
                fifo_pending.push_back(hit.clone());
            }
            fifo_pending.retain(|hit| timestamp - hit.timestamp <= 1.0);
            for current_hp in hp_updates {
                let previous_hp =
                    fifo_last_hp.or_else(|| fifo_pending.front().map(|hit| hit.target_hp_before));
                fifo_last_hp = Some(*current_hp);
                let Some(previous_hp) = previous_hp else {
                    continue;
                };
                let actual = previous_hp - current_hp;
                if actual <= 0.0 {
                    continue;
                }
                let Some(hit) = fifo_pending.pop_front() else {
                    continue;
                };
                let residual = actual - hit.damage;
                if residual > 0.0 {
                    fifo_positive_residual += residual;
                    let ratio = residual / hit.damage;
                    if (0.18..=0.26).contains(&ratio) {
                        fifo_overlay_residual += residual;
                    }
                } else {
                    fifo_negative_residual += residual;
                }
            }
        }
        println!(
            "fifo_summary positive={fifo_positive_residual:.0} overlay={fifo_overlay_residual:.0} negative={fifo_negative_residual:.0} pending={:.0}",
            fifo_pending.iter().map(|hit| hit.damage).sum::<f64>()
        );

        let mut pending = Vec::<Hit>::new();
        let mut last_hp = None::<f64>;
        for (timestamp, hits, hp_updates) in timeline {
            for hit in hits {
                println!(
                    "settlement_hit t={timestamp:.6} char={} damage={:.0} hp_before={:.0}",
                    hit.char_id, hit.damage, hit.target_hp_before
                );
                if pending
                    .last()
                    .is_some_and(|previous| timestamp - previous.timestamp > 1.0)
                {
                    println!("settlement_clear reason=hit_gap pending={}", pending.len());
                    pending.clear();
                }
                pending.push(hit);
            }
            for current_hp in hp_updates {
                pending.retain(|hit| timestamp - hit.timestamp <= 1.0);
                let previous_hp =
                    last_hp.or_else(|| pending.first().map(|hit| hit.target_hp_before));
                last_hp = Some(current_hp);
                let Some(previous_hp) = previous_hp else {
                    println!(
                        "settlement_hp t={timestamp:.6} hp={current_hp:.0} reason=no_baseline"
                    );
                    continue;
                };
                if current_hp >= previous_hp || pending.is_empty() {
                    println!(
                        "settlement_hp t={timestamp:.6} hp={current_hp:.0} previous={previous_hp:.0} pending={} reason={}",
                        pending.len(),
                        if current_hp >= previous_hp {
                            "not_damage"
                        } else {
                            "no_pending_hit"
                        }
                    );
                    if current_hp > previous_hp {
                        pending.clear();
                    }
                    continue;
                }
                let settled = std::mem::take(&mut pending);
                let reported = settled.iter().map(|hit| hit.damage).sum::<f64>();
                let actual = previous_hp - current_hp;
                let residual = actual - reported;
                let ratio = residual / reported;
                println!(
                    "settlement t={timestamp:.6} previous={previous_hp:.0} hp={current_hp:.0} actual={actual:.0} reported={reported:.0} residual={residual:.0} ratio={:.1}% chars={:?} accepted={}",
                    ratio * 100.0,
                    settled.iter().map(|hit| hit.char_id).collect::<Vec<_>>(),
                    residual >= 1.0 && (0.10..=0.30).contains(&ratio)
                );
            }
        }
    }
}
