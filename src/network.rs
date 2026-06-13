use std::mem::size_of;
use std::net::Ipv4Addr;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
use windows_sys::Win32::Networking::WinSock::AF_INET;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

use crate::capture::CaptureDevice;

const GAME_PROCESS: &str = "HTGame.exe";
const GAME_TCP_PORT: u16 = 30031;
const MIB_TCP_STATE_ESTABLISHED: u32 = 5;

#[derive(Clone, Debug)]
pub struct GameNetwork {
    pub pid: u32,
    pub local_ip: Ipv4Addr,
    pub remote_ip: Ipv4Addr,
    pub remote_port: u16,
}

pub fn detect_game_device(devices: &[CaptureDevice]) -> Result<(usize, GameNetwork), String> {
    let pid =
        find_process_id(GAME_PROCESS)?.ok_or_else(|| format!("未检测到游戏进程 {GAME_PROCESS}"))?;
    let connections = tcp_connections_for_pid(pid)?;
    let network = connections
        .iter()
        .find(|row| row.remote_port == GAME_TCP_PORT)
        .or_else(|| connections.first())
        .cloned()
        .ok_or_else(|| {
            format!("已检测到 {GAME_PROCESS} (PID {pid})，但尚未建立可用于定位网卡的 IPv4 TCP 连接")
        })?;
    let device_index = devices
        .iter()
        .position(|device| device.ipv4.contains(&network.local_ip))
        .ok_or_else(|| {
            format!(
                "游戏使用本机 IP {}，但 Npcap 设备列表中没有对应网卡",
                network.local_ip
            )
        })?;
    Ok((device_index, network))
}

fn find_process_id(executable: &str) -> Result<Option<u32>, String> {
    // SAFETY: Toolhelp snapshot functions are called with initialized structures and closed below.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err("无法枚举系统进程".to_owned());
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let length = entry
                    .szExeFile
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
                if name.eq_ignore_ascii_case(executable) {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        Ok(found)
    }
}

fn tcp_connections_for_pid(pid: u32) -> Result<Vec<GameNetwork>, String> {
    // SAFETY: The buffer size is obtained from GetExtendedTcpTable and rows are read unaligned
    // because the table starts immediately after its 32-bit count.
    unsafe {
        let mut size = 0_u32;
        let first = GetExtendedTcpTable(
            ptr::null_mut(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        if first != ERROR_INSUFFICIENT_BUFFER {
            return Err(format!("读取 TCP 连接表大小失败，错误码 {first}"));
        }
        let mut buffer = vec![0_u8; size as usize];
        let result = GetExtendedTcpTable(
            buffer.as_mut_ptr().cast(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        if result != 0 {
            return Err(format!("读取 TCP 连接表失败，错误码 {result}"));
        }
        if buffer.len() < size_of::<u32>() {
            return Ok(Vec::new());
        }
        let count = ptr::read_unaligned(buffer.as_ptr().cast::<u32>()) as usize;
        let rows_start = buffer.as_ptr().add(size_of::<u32>());
        let available_rows =
            buffer.len().saturating_sub(size_of::<u32>()) / size_of::<MIB_TCPROW_OWNER_PID>();
        let mut connections = Vec::new();
        for index in 0..count.min(available_rows) {
            let row = ptr::read_unaligned(
                rows_start
                    .add(index * size_of::<MIB_TCPROW_OWNER_PID>())
                    .cast::<MIB_TCPROW_OWNER_PID>(),
            );
            if row.dwOwningPid != pid || row.dwState != MIB_TCP_STATE_ESTABLISHED {
                continue;
            }
            let local_ip = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
            let remote_ip = Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes());
            if local_ip.is_loopback() || local_ip.is_unspecified() || remote_ip.is_unspecified() {
                continue;
            }
            connections.push(GameNetwork {
                pid,
                local_ip,
                remote_ip,
                remote_port: decode_port(row.dwRemotePort),
            });
        }
        connections.sort_by_key(|row| (row.remote_port != GAME_TCP_PORT, row.remote_port));
        Ok(connections)
    }
}

fn decode_port(value: u32) -> u16 {
    u16::from_be(value as u16)
}
