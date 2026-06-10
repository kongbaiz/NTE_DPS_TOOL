use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_uint};
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

use crate::model::{CharacterInfo, EngineEvent, PacketDebug};
use crate::parser::{
    declared_character_ids, find_declared_character_evidence, parse_damage_payload,
};

const PCAP_ERRBUF_SIZE: usize = 256;

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
}

impl CaptureHandle {
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
    let thread = thread::spawn(move || {
        if let Err(error) = run_capture(
            &device,
            local_ip,
            &filter,
            include_incoming,
            &characters,
            &sender,
            &thread_stop,
        ) {
            let _ = sender.send(EngineEvent::Error(error));
        }
        let _ = sender.send(EngineEvent::CaptureStopped);
    });
    CaptureHandle {
        stop,
        thread: Some(thread),
    }
}

fn run_capture(
    device: &CaptureDevice,
    local_ip: Option<Ipv4Addr>,
    filter: &str,
    include_incoming: bool,
    characters: &HashMap<u32, CharacterInfo>,
    sender: &Sender<EngineEvent>,
    stop: &AtomicBool,
) -> Result<(), String> {
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
        let _ = sender.send(EngineEvent::Status(format!(
            "正在抓包: {} ({})",
            device.description,
            local_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "不过滤本机 IP".to_owned())
        )));

        let mut session_characters: HashMap<(Ipv4Addr, u16, Ipv4Addr, u16), u32> = HashMap::new();
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
            let Some((src, src_port, dst, dst_port, payload)) = parse_udp_ipv4(packet) else {
                continue;
            };
            if local_ip.is_some_and(|ip| src != ip) {
                continue;
            }
            let timestamp =
                header_ref.ts.tv_sec as f64 + header_ref.ts.tv_usec as f64 / 1_000_000.0;
            let evidence = find_declared_character_evidence(payload);
            let ids = declared_character_ids(payload);
            let packet_char_id = if ids.len() == 1 {
                ids.first().copied()
            } else {
                None
            };
            let session_key = (src, src_port, dst, dst_port);
            if let Some(id) = packet_char_id {
                session_characters.insert(session_key, id);
            }
            let fallback = session_characters.get(&session_key).copied();
            let hits = parse_damage_payload(
                payload,
                timestamp,
                packet_char_id,
                fallback,
                characters,
                &evidence,
            );
            let accepted = hits
                .iter()
                .filter(|hit| include_incoming || hit.direction != "incoming")
                .count();
            if !hits.is_empty() || !ids.is_empty() {
                let preview_len = payload.len().min(96);
                let _ = sender.send(EngineEvent::Packet(PacketDebug {
                    timestamp,
                    source: format!("{src}:{src_port}"),
                    destination: format!("{dst}:{dst_port}"),
                    payload_len: payload.len(),
                    declared_ids: ids,
                    parsed_hits: accepted,
                    note: if hits.len() != accepted {
                        format!("过滤 {} 条 incoming 记录", hits.len() - accepted)
                    } else {
                        String::new()
                    },
                    payload_preview: hex::encode(&payload[..preview_len]),
                }));
            }
            for hit in hits {
                if include_incoming || hit.direction != "incoming" {
                    let _ = sender.send(EngineEvent::Hit(hit));
                }
            }
        }
        close(handle);
    }
    Ok(())
}

pub fn replay_hits(
    path: PathBuf,
    sender: Sender<EngineEvent>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
            let mut previous_timestamp: Option<f64> = None;
            for (index, line) in text.lines().enumerate() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let hit: crate::model::Hit = serde_json::from_str(line)
                    .map_err(|error| format!("第 {} 行解析失败: {error}", index + 1))?;
                if let Some(previous) = previous_timestamp {
                    let delay = (hit.timestamp - previous).clamp(0.0, 0.25);
                    thread::sleep(Duration::from_secs_f64(delay));
                }
                previous_timestamp = Some(hit.timestamp);
                sender
                    .send(EngineEvent::Hit(hit))
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                let _ = sender.send(EngineEvent::Status(format!("回放完成: {}", path.display())));
            }
            Err(error) => {
                let _ = sender.send(EngineEvent::Error(error));
            }
        }
        let _ = sender.send(EngineEvent::CaptureStopped);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_truncated_packets() {
        assert!(parse_udp_ipv4(&[]).is_none());
        assert!(parse_udp_ipv4(&[0_u8; 13]).is_none());
        assert!(parse_udp_ipv4(&[0_u8; 42]).is_none());
    }
}
