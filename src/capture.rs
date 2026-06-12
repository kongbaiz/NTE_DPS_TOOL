use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_uint};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::ptr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

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
    declared_character_ids, find_declared_character_evidence, parse_damage_payload,
};

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
    raw_capture_path: PathBuf,
}

impl CaptureHandle {
    pub fn raw_capture_path(&self) -> &std::path::Path {
        &self.raw_capture_path
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
    PathBuf::from(r"C:\Windows\System32\Npcap\wpcap.dll")
}

fn packet_library_path() -> PathBuf {
    PathBuf::from(r"C:\Windows\System32\Npcap\Packet.dll")
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
    if ip[0] >> 4 != 4 || ip_header_len < 20 || ip.len() < ip_header_len + 8 || ip[9] != 17 {
        return None;
    }
    let source = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let destination = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
    let udp = &ip[ip_header_len..];
    let source_port = u16::from_be_bytes([udp[0], udp[1]]);
    let destination_port = u16::from_be_bytes([udp[2], udp[3]]);
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    let payload_end = udp_len.min(udp.len());
    if payload_end < 8 {
        return None;
    }
    Some((
        source,
        source_port,
        destination,
        destination_port,
        &udp[8..payload_end],
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
    let _ = sender.send(EngineEvent::Packet(packet));
}

#[derive(Default)]
struct PacketDecoder {
    session_characters: HashMap<(Ipv4Addr, u16, Ipv4Addr, u16), u32>,
    client_endpoints: HashSet<(Ipv4Addr, u16)>,
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
        let ids = declared_character_ids(payload);
        let outgoing = infer_outgoing(src, src_port, dst, local_ip, &ids, &self.client_endpoints);
        if outgoing && !ids.is_empty() {
            self.client_endpoints.insert((src, src_port));
        }
        let direction = if outgoing { "C2S" } else { "S2C" };
        let hits = if outgoing {
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
        let accepted = hits
            .iter()
            .filter(|hit| include_incoming || hit.direction != "incoming")
            .count();
        let preview_len = payload.len().min(96);
        let payload_hex = hex::encode(payload);
        let decoded_text = decode_payload_text(payload);
        if !should_keep_debug_packet(payload, &ids, accepted, &decoded_text) {
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
                let _ = sender.send(EngineEvent::Hit(hit));
            }
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
    raw_capture_path: PathBuf,
) -> CaptureHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread_raw_capture_path = raw_capture_path.clone();
    let thread = thread::spawn(move || {
        if let Err(error) = run_capture(CaptureRunConfig {
            device: &device,
            local_ip,
            filter: &filter,
            include_incoming,
            characters: &characters,
            sender: &sender,
            stop: &thread_stop,
            raw_capture_path: &thread_raw_capture_path,
        }) {
            let _ = sender.send(EngineEvent::Error(error));
        }
        let _ = sender.send(EngineEvent::CaptureStopped);
    });
    CaptureHandle {
        stop,
        thread: Some(thread),
        raw_capture_path,
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
    raw_capture_path: &'a std::path::Path,
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
        raw_capture_path,
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
        let mut raw_capture = match RawCaptureWriter::create(raw_capture_path, device) {
            Ok(writer) => {
                let _ = sender.send(EngineEvent::Status(format!(
                    "正在抓包，完整原始帧同步保存至 {}",
                    raw_capture_path.display()
                )));
                Some(writer)
            }
            Err(error) => {
                let _ = sender.send(EngineEvent::Status(format!(
                    "原始抓包保存不可用，实时解析继续：{error}"
                )));
                None
            }
        };
        let raw_capture_status = if raw_capture.is_some() {
            raw_capture_path.display().to_string()
        } else {
            "不可用（实时解析继续）".to_owned()
        };
        let _ = sender.send(EngineEvent::Status(format!(
            "正在抓包: {} ({})；原始文件: {}",
            device.description,
            local_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "不过滤本机 IP".to_owned()),
            raw_capture_status
        )));

        let mut decoder = PacketDecoder::default();
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
            if let Some(writer) = raw_capture.as_mut() {
                let raw_timestamp = Duration::new(
                    header_ref.ts.tv_sec.max(0) as u64,
                    header_ref.ts.tv_usec.clamp(0, 999_999) as u32 * 1_000,
                );
                if let Err(error) = writer.write_packet(raw_timestamp, header_ref.len, packet) {
                    let _ = sender.send(EngineEvent::Status(format!(
                        "原始抓包写入失败，实时解析继续：{error}"
                    )));
                    raw_capture = None;
                }
            }
            decoder.process_ethernet_frame(
                packet,
                timestamp,
                local_ip,
                include_incoming,
                characters,
                sender,
            );
        }
        if let Some(writer) = raw_capture {
            match writer.finish() {
                Ok((packet_count, captured_bytes)) => {
                    let _ = sender.send(EngineEvent::Status(format!(
                        "完整抓包已保存：{}，{} 包，{} 字节",
                        raw_capture_path.display(),
                        packet_count,
                        captured_bytes
                    )));
                }
                Err(error) => {
                    let _ = sender.send(EngineEvent::Status(format!(
                        "原始抓包刷新失败，实时解析结果不受影响：{error}"
                    )));
                }
            }
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
            let document = parse_capture_export(&text)?;
            let hit_count = document.hits.len();
            let mut packet_count = 0;
            let mut events = Vec::<(f64, u8, EngineEvent)>::new();

            for packet in document.packets {
                let declared_ids = parse_export_ids(&packet.declared_ids);
                let payload = hex::decode(&packet.payload_hex).unwrap_or_default();
                let decoded_text = if payload.is_empty() {
                    packet.decoded_text
                } else {
                    decode_payload_text(&payload)
                };
                if !should_keep_debug_packet(
                    &payload,
                    &declared_ids,
                    packet.parsed_hits,
                    &decoded_text,
                ) {
                    continue;
                }
                let evidence = find_declared_character_evidence(&payload);
                let mut note = packet.note;
                append_packet_note(
                    &mut note,
                    binary_payload_diagnostic(
                        &payload,
                        &packet.direction,
                        &decoded_text,
                        &evidence,
                    ),
                );
                packet_count += 1;
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
                    events.push((packet.timestamp, 0, EngineEvent::Abyss(event)));
                }
                events.push((packet.timestamp, 1, EngineEvent::Packet(packet)));
            }
            for hit in document.hits {
                let timestamp = hit.timestamp_unix;
                events.push((
                    timestamp,
                    2,
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
                    }),
                ));
            }
            events.sort_by(|left, right| {
                left.0
                    .total_cmp(&right.0)
                    .then_with(|| left.1.cmp(&right.1))
            });
            for (_, _, event) in events {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                sender.send(event).map_err(|error| error.to_string())?;
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
            .map(|value| value as u32)
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

    fn udp_ipv4_frame(source: Ipv4Addr, destination: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0_u8; 14 + 20 + 8 + payload.len()];
        frame[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
        let ip = &mut frame[14..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((20 + 8 + payload.len()) as u16).to_be_bytes());
        ip[9] = 17;
        ip[12..16].copy_from_slice(&source.octets());
        ip[16..20].copy_from_slice(&destination.octets());
        let udp = &mut ip[20..];
        udp[0..2].copy_from_slice(&64592_u16.to_be_bytes());
        udp[2..4].copy_from_slice(&30216_u16.to_be_bytes());
        udp[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        udp[8..].copy_from_slice(payload);
        frame
    }

    #[test]
    fn writes_complete_ethernet_frames_to_pcapng() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nte-raw-capture-{}-{unique}.pcapng",
            std::process::id()
        ));
        let device = CaptureDevice {
            name: "test-device".to_owned(),
            description: "test adapter".to_owned(),
            ipv4: vec![Ipv4Addr::LOCALHOST],
        };
        let packet: Vec<u8> = (0..96).collect();
        let timestamp = Duration::new(1_781_017_376, 744_123_000);

        let mut writer = RawCaptureWriter::create(&path, &device).unwrap();
        writer.write_packet(timestamp, 128, &packet).unwrap();
        assert_eq!(writer.finish().unwrap(), (1, packet.len() as u64));

        let file = File::open(&path).unwrap();
        let mut reader = PcapNgReader::new(file).unwrap();
        let mut interface_seen = false;
        let mut packet_seen = false;
        while let Some(block) = reader.next_block() {
            match block.unwrap() {
                Block::InterfaceDescription(interface) => {
                    interface_seen = true;
                    assert_eq!(interface.linktype, DataLink::ETHERNET);
                    assert_eq!(interface.snaplen, CAPTURE_SNAPLEN);
                    assert!(
                        interface
                            .options
                            .contains(&InterfaceDescriptionOption::IfTsResol(9))
                    );
                }
                Block::EnhancedPacket(captured) => {
                    packet_seen = true;
                    assert_eq!(captured.timestamp, timestamp);
                    assert_eq!(captured.original_len, 128);
                    assert_eq!(captured.data.as_ref(), packet);
                }
                _ => {}
            }
        }
        assert!(interface_seen);
        assert!(packet_seen);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_empty_and_truncated_packets() {
        assert!(parse_udp_ipv4(&[]).is_none());
        assert!(parse_udp_ipv4(&[0_u8; 13]).is_none());
        assert!(parse_udp_ipv4(&[0_u8; 42]).is_none());
    }

    #[test]
    fn imports_recorded_pcapng_into_debug_events() {
        let path = PathBuf::from("data/1.pcapng");
        if !path.is_file() {
            return;
        }
        let characters = Arc::new(
            crate::parser::load_characters(std::path::Path::new("characters.json")).unwrap(),
        );
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        import_pcapng(path, characters, false, sender, stop)
            .join()
            .unwrap();

        let events: Vec<_> = receiver.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Packet(_)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Hit(_)))
        );
    }

    #[test]
    fn keeps_completed_first_half_when_battle_born_starts_second_half() {
        let path = PathBuf::from("logs/1.pcapng");
        if !path.is_file() {
            return;
        }
        let characters = Arc::new(
            crate::parser::load_characters(std::path::Path::new("characters.json")).unwrap(),
        );
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        import_pcapng(path, characters, true, sender, stop)
            .join()
            .unwrap();
        let mut state = crate::model::CombatState::default();
        for event in receiver.try_iter() {
            match event {
                EngineEvent::Abyss(event) => state.apply_abyss_event(event),
                EngineEvent::Hit(hit) => state.push_hit(hit),
                _ => {}
            }
        }

        assert_eq!(state.abyss.floor, Some(12));
        assert_eq!(state.abyss.first_half.hits.len(), 212);
        assert_eq!(state.abyss.first_half.total_damage, 1_926_902.0);
        assert_eq!(state.abyss.first_half.total_damage_taken, 5_259.0);
        assert!(state.abyss.second_half.hits.is_empty());
    }

    #[test]
    fn imports_exported_capture_json_and_repairs_legacy_comma() {
        let text = r#"{
  "hits": [{
    "timestamp_unix": 1.25,
    "char_id": 1010,
    "char_name": "娜娜莉",
    "damage": 1234,
    "target_hp_after": 8000,
    "target_max_hp": 10000,
    "target_hp_percent": 80
  }],
  "packets": [{
    "timestamp_unix": 1.25,
    "source": "127.0.0.1:1",
    "destination": "127.0.0.1:2",
    "direction": "C2S",
    "payload_len": 2,
    "declared_ids": "[1010]",
    "parsed_hits": 1,
    "note": "",
    "payload_preview": "abcd",
    "payload_hex": "abcd"
    "decoded_text": "Abyss_2_12_0"
  }]
}"#;
        let document = parse_capture_export(text).unwrap();
        assert_eq!(document.hits.len(), 1);
        assert_eq!(document.packets.len(), 1);
        assert_eq!(
            parse_export_ids(&document.packets[0].declared_ids),
            vec![1010]
        );
    }

    #[test]
    fn preserves_complete_udp_payload() {
        let payload: Vec<u8> = (0..=255).cycle().take(900).collect();
        let frame = udp_ipv4_frame(
            Ipv4Addr::new(192, 168, 31, 61),
            Ipv4Addr::new(49, 232, 46, 87),
            &payload,
        );
        let (_, _, _, _, parsed_payload) = parse_udp_ipv4(&frame).expect("valid UDP frame");

        assert_eq!(parsed_payload, payload);
        assert_eq!(hex::encode(parsed_payload).len(), payload.len() * 2);
    }

    #[test]
    fn infers_private_address_as_client_without_detected_local_ip() {
        let private = Ipv4Addr::new(192, 168, 31, 61);
        let public = Ipv4Addr::new(49, 232, 46, 87);
        let endpoints = HashSet::new();

        assert!(infer_outgoing(
            private,
            64592,
            public,
            None,
            &[1010],
            &endpoints,
        ));
        assert!(!infer_outgoing(
            public,
            30216,
            private,
            None,
            &[1010],
            &endpoints,
        ));
    }

    #[test]
    fn extracts_shifted_protocol_text() {
        let source = b"FAbyssGamePlayData\0Abyss_2_12_0\0EAbyssFightStage::FirstHalf";
        let mut shifted = Vec::with_capacity(source.len() + 1);
        shifted.push(source[0] << 4);
        for pair in source.windows(2) {
            shifted.push((pair[0] >> 4) | (pair[1] << 4));
        }
        shifted.push(source[source.len() - 1] >> 4);

        let decoded = decode_payload_text(&shifted);
        assert!(decoded.contains("FAbyssGamePlayData"));
        assert!(decoded.contains("Abyss_2_12_0"));
        assert!(decoded.contains("EAbyssFightStage::FirstHalf"));
    }

    #[test]
    fn extracts_shifted_length_prefixed_identifier() {
        let value = b"EvadeBeanAdd\0";
        let mut source = Vec::new();
        source.extend_from_slice(&(value.len() as u32).to_le_bytes());
        source.extend_from_slice(value);
        let mut shifted = Vec::with_capacity(source.len() + 1);
        shifted.push(source[0] << 3);
        for pair in source.windows(2) {
            shifted.push((pair[0] >> 5) | (pair[1] << 3));
        }
        shifted.push(source[source.len() - 1] >> 5);

        assert_eq!(decode_payload_text(&shifted), "EvadeBeanAdd");
    }

    #[test]
    fn describes_unreadable_replication_fragments_without_inventing_fields() {
        let payload = vec![0_u8; 400];
        let evidence = vec![(1010, 7, 30), (1010, 3, 145)];
        let diagnostic =
            binary_payload_diagnostic(&payload, "S2C", UNREADABLE_PROTOCOL_TEXT, &evidence)
                .unwrap();

        assert!(diagnostic.contains("无内联字段名"));
        assert!(diagnostic.contains("2 个锚点、2 种位对齐"));
        assert!(diagnostic.contains("1010@bit247"));
        assert!(diagnostic.contains("1010@bit1163"));
        assert!(
            binary_payload_diagnostic(&payload, "C2S", UNREADABLE_PROTOCOL_TEXT, &evidence)
                .is_none()
        );
        assert!(binary_payload_diagnostic(&payload, "S2C", "UnbalCurrent", &evidence).is_none());
    }

    #[test]
    fn marks_low_entropy_long_s2c_payload_as_candidate_only() {
        let mut payload = vec![0_u8; 400];
        for index in (0..payload.len()).step_by(16) {
            payload[index] = (index / 16) as u8 + 1;
        }
        let diagnostic =
            binary_payload_diagnostic(&payload, "S2C", UNREADABLE_PROTOCOL_TEXT, &[]).unwrap();

        assert!(diagnostic.starts_with("候选 UE 位打包复制/引用增量"));
        assert!(diagnostic.contains("零字节占比"));
        assert!(diagnostic.contains("熵"));
    }

    #[test]
    fn recovers_action_names_from_current_capture() {
        let path = std::path::Path::new("logs/nte_capture_20260612_114658.json");
        if !path.is_file() {
            return;
        }
        let document = parse_capture_export(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut decoded_packets = Vec::new();
        let mut confirmed_binary_deltas = 0;
        let mut candidate_binary_deltas = 0;
        for packet in document.packets {
            let payload = hex::decode(packet.payload_hex).unwrap();
            let text = decode_payload_text(&payload);
            if text != UNREADABLE_PROTOCOL_TEXT {
                decoded_packets.push(text.clone());
            }
            let evidence = find_declared_character_evidence(&payload);
            if let Some(diagnostic) =
                binary_payload_diagnostic(&payload, &packet.direction, &text, &evidence)
            {
                if diagnostic.starts_with("已识别") {
                    confirmed_binary_deltas += 1;
                } else if diagnostic.starts_with("候选") {
                    candidate_binary_deltas += 1;
                }
            }
        }

        assert_eq!(confirmed_binary_deltas, 86);
        assert_eq!(candidate_binary_deltas, 5);
        assert!(decoded_packets.iter().any(|value| value.contains("Melee1")));
        assert!(
            decoded_packets
                .iter()
                .any(|value| value.contains("PerfectEvadeFront"))
        );
        assert!(
            decoded_packets
                .iter()
                .any(|value| value.contains("CritDamageBase"))
        );
    }

    #[test]
    fn rejects_shifted_garbage_text() {
        for value in [
            "Zbjdrpp\\`hl```Xddjfpd\\bnd```",
            ":xB\"xB\"xB",
            "bjrjhjjnrj",
            "AAhw?*vF",
            "ZKccsQJss",
        ] {
            assert_eq!(protocol_text_score(value), 0, "{value}");
        }
    }

    #[test]
    fn keeps_scene_and_unreal_protocol_identifiers() {
        for value in [
            "CurrentGameplayID",
            "WorldBoss_Boss13",
            "TeleportWithCar",
            "CityLive",
            "FHTClientActiveGE",
            "FCharacterForNet",
            "/Game/Maps/Map_bigworld/XL_map_bigworld_test",
        ] {
            assert!(protocol_text_score(value) > 0, "{value}");
        }
    }

    #[test]
    fn filters_only_short_unparsed_debug_packets() {
        let long_binary: Vec<u8> = (0..128).map(|value| value as u8).collect();
        assert!(!should_keep_debug_packet(
            &[0xff; 30],
            &[],
            0,
            UNREADABLE_PROTOCOL_TEXT,
        ));
        assert!(!should_keep_debug_packet(
            &[0x10; 48],
            &[],
            0,
            UNREADABLE_PROTOCOL_TEXT,
        ));
        assert!(should_keep_debug_packet(
            &long_binary,
            &[],
            0,
            UNREADABLE_PROTOCOL_TEXT,
        ));
        assert!(should_keep_debug_packet(
            &[0x10; 11],
            &[],
            1,
            UNREADABLE_PROTOCOL_TEXT,
        ));
        assert!(should_keep_debug_packet(&[0x10; 11], &[], 0, "CityLive",));
    }

    #[test]
    fn recognizes_reliable_abyss_stage_events() {
        let first = abyss_events_from_text(
            1.0,
            "EAbyssFightStage::FirstHalf\nFAbyssGamePlayData\nAbyss_2_12_0",
        );
        let second = abyss_events_from_text(
            2.0,
            "EAbyssFightStage::SecondHalf\nFAbyssGamePlayData\nAbyss_2_12_1",
        );
        let clone_data = abyss_events_from_text(
            3.0,
            "EAbyssFightStage::FirstHalf\nEAbyssFightStage::SecondHalf\nAbyssCloneCharacterData",
        );
        let success = abyss_events_from_text(
            4.0,
            "EAbyssFightStage::SecondHalf\nFAbyssGamePlayData\nConditionState_Success",
        );
        let restart = abyss_events_from_text(5.0, "Abyss_Battle_Born\nXL_map_bigworld_test");
        let restart_with_stale_stage = abyss_events_from_text(
            6.0,
            "EAbyssFightStage::FirstHalf\nFAbyssGamePlayData\nAbyss_Battle_Born\nAbyss_2",
        );

        assert!(matches!(
            first.as_slice(),
            [AbyssEvent::Stage {
                floor: Some(12),
                half: AbyssHalf::First,
                ..
            }]
        ));
        assert!(matches!(
            second.as_slice(),
            [AbyssEvent::Stage {
                floor: Some(12),
                half: AbyssHalf::Second,
                ..
            }]
        ));
        assert!(clone_data.is_empty());
        assert!(matches!(success.as_slice(), [AbyssEvent::Success { .. }]));
        assert!(matches!(
            restart.as_slice(),
            [AbyssEvent::RestartDetected { .. }]
        ));
        assert!(matches!(
            restart_with_stale_stage.as_slice(),
            [AbyssEvent::RestartDetected { .. }]
        ));
    }

    #[test]
    fn imports_latest_abyss_capture_into_two_parties() {
        let path = PathBuf::from("data/nte_capture_20260611_214538.json");
        if !path.is_file() {
            return;
        }
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        import_capture_json(path, sender, stop).join().unwrap();
        let mut state = crate::model::CombatState::default();
        for event in receiver.try_iter() {
            match event {
                EngineEvent::Abyss(event) => state.apply_abyss_event(event),
                EngineEvent::Hit(hit) => state.push_hit(hit),
                _ => {}
            }
        }

        assert_eq!(state.abyss.floor, Some(12));
        assert_eq!(state.abyss.first_half.hits.len(), 219);
        assert_eq!(state.abyss.second_half.hits.len(), 233);
        assert!(state.abyss.first_half.stats.contains_key(&1010));
        assert!(state.abyss.second_half.stats.contains_key(&1004));
        assert!(state.abyss.success_at.is_some());
        assert!(state.abyss.exited_at.is_some());
    }
}
