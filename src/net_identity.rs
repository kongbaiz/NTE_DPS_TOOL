use std::collections::HashSet;

use crate::object_state::{is_ignored_non_target_path, is_targetish_path};
use crate::ue_bitstream::{PathCandidate, decode_shifted_bytes};

const ANCHOR_WINDOW_BEFORE: usize = 24;
const ANCHOR_WINDOW_AFTER: usize = 0;
const MIN_CANDIDATE_SCORE: u16 = 40;
const MAX_CANDIDATES_PER_PATH: usize = 4;
const MAX_TOTAL_CANDIDATES: usize = 24;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NetIdentityCandidateKind {
    NetGuidPacked,
    NetGuid32,
    IrisNetRefHandle32,
}

impl NetIdentityCandidateKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::NetGuidPacked => "netguid_packed",
            Self::NetGuid32 => "netguid32",
            Self::IrisNetRefHandle32 => "iris_ref32",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetIdentityCandidate {
    pub kind: NetIdentityCandidateKind,
    pub handle: String,
    pub path: String,
    pub byte_offset: usize,
    pub bit_shift: u8,
    pub relative_offset: isize,
    pub raw_hex: String,
    pub score: u16,
}

pub fn extract_net_identity_candidates(
    data: &[u8],
    paths: &[PathCandidate],
) -> Vec<NetIdentityCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for path in paths
        .iter()
        .filter(|path| !is_ignored_non_target_path(&path.value) && is_targetish_path(&path.value))
        .take(8)
    {
        let Some(shifted) = decode_shifted_bytes(
            data,
            0,
            path.bit_shift,
            0,
            data.len().saturating_sub(usize::from(path.bit_shift != 0)),
        ) else {
            continue;
        };
        if path.byte_offset >= shifted.len() {
            continue;
        }

        let anchor_offset = adjusted_anchor_offset(&shifted, path);
        let mut path_candidates = Vec::new();
        collect_length_prefixed_candidates(&shifted, path, anchor_offset, &mut path_candidates);
        collect_window_candidates(&shifted, path, anchor_offset, &mut path_candidates);

        path_candidates.retain(|candidate| candidate.score >= MIN_CANDIDATE_SCORE);
        path_candidates.sort_by_key(|candidate| {
            (
                std::cmp::Reverse(candidate.score),
                candidate.relative_offset.abs(),
                candidate.byte_offset,
            )
        });
        path_candidates.truncate(MAX_CANDIDATES_PER_PATH);

        for candidate in path_candidates {
            let key = (
                candidate.kind,
                candidate.handle.clone(),
                candidate.path.clone(),
                candidate.bit_shift,
                candidate.byte_offset,
            );
            if seen.insert(key) {
                candidates.push(candidate);
            }
        }
    }

    candidates.sort_by_key(|candidate| {
        (
            std::cmp::Reverse(candidate.score),
            candidate.bit_shift,
            candidate.byte_offset,
        )
    });
    candidates.truncate(MAX_TOTAL_CANDIDATES);
    candidates
}

fn adjusted_anchor_offset(data: &[u8], path: &PathCandidate) -> usize {
    let path_bytes = path.value.as_bytes();
    if data.get(path.byte_offset..path.byte_offset.saturating_add(path_bytes.len()))
        == Some(path_bytes)
    {
        return path.byte_offset;
    }
    let start = path.byte_offset.saturating_sub(4);
    let end = path
        .byte_offset
        .saturating_add(4)
        .min(data.len().saturating_sub(path_bytes.len()));
    (start..=end)
        .find(|offset| {
            data.get(*offset..offset.saturating_add(path_bytes.len())) == Some(path_bytes)
        })
        .unwrap_or(path.byte_offset)
}

fn collect_length_prefixed_candidates(
    data: &[u8],
    path: &PathCandidate,
    anchor_offset: usize,
    candidates: &mut Vec<NetIdentityCandidate>,
) {
    let Some(length_offset) = anchor_offset.checked_sub(4) else {
        return;
    };
    let Some(length_bytes) = data.get(length_offset..length_offset + 4) else {
        return;
    };
    let length = u32::from_le_bytes(length_bytes.try_into().unwrap()) as usize;
    let expected_with_nul = path.value.len() + 1;
    if length != path.value.len() && length != expected_with_nul {
        return;
    }

    if let Some(value_offset) = length_offset.checked_sub(4) {
        push_u32_candidate(
            data,
            path,
            candidates,
            NetIdentityCandidateKind::NetGuid32,
            value_offset,
            anchor_offset,
            82,
        );
    }
    if let Some(value_offset) = length_offset.checked_sub(8) {
        push_u32_candidate(
            data,
            path,
            candidates,
            NetIdentityCandidateKind::IrisNetRefHandle32,
            value_offset,
            anchor_offset,
            72,
        );
    }
    if let Some(value_offset) = length_offset.checked_sub(12) {
        push_u32_candidate(
            data,
            path,
            candidates,
            NetIdentityCandidateKind::IrisNetRefHandle32,
            value_offset,
            anchor_offset,
            66,
        );
    }
}

fn collect_window_candidates(
    data: &[u8],
    path: &PathCandidate,
    anchor_offset: usize,
    candidates: &mut Vec<NetIdentityCandidate>,
) {
    let start = anchor_offset.saturating_sub(ANCHOR_WINDOW_BEFORE);
    let scan_end = identity_scan_end(data, path, anchor_offset);
    let end = scan_end.saturating_add(ANCHOR_WINDOW_AFTER).min(data.len());
    for offset in start..end {
        if offset + 4 <= scan_end && offset + 4 <= data.len() {
            push_u32_candidate(
                data,
                path,
                candidates,
                NetIdentityCandidateKind::IrisNetRefHandle32,
                offset,
                anchor_offset,
                38,
            );
        }
        if let Some((value, width)) = read_serialized_int_packed(data, offset)
            && offset + width <= scan_end
            && plausible_packed_value(value, &data[offset..offset + width], path)
        {
            let raw = &data[offset..offset + width];
            push_candidate(
                path,
                candidates,
                NetIdentityCandidateKind::NetGuidPacked,
                format!("0x{value:x}"),
                offset,
                anchor_offset,
                hex::encode(raw),
                packed_score(offset, anchor_offset, width),
            );
        }
    }
}

fn identity_scan_end(data: &[u8], path: &PathCandidate, anchor_offset: usize) -> usize {
    let Some(length_offset) = anchor_offset.checked_sub(4) else {
        return anchor_offset;
    };
    let Some(length_bytes) = data.get(length_offset..length_offset + 4) else {
        return anchor_offset;
    };
    let length = u32::from_le_bytes(length_bytes.try_into().unwrap()) as usize;
    if length == path.value.len() || length == path.value.len() + 1 {
        length_offset
    } else {
        anchor_offset
    }
}

fn push_u32_candidate(
    data: &[u8],
    path: &PathCandidate,
    candidates: &mut Vec<NetIdentityCandidate>,
    kind: NetIdentityCandidateKind,
    value_offset: usize,
    anchor_offset: usize,
    score: u16,
) {
    let Some(raw) = data.get(value_offset..value_offset + 4) else {
        return;
    };
    let value = u32::from_le_bytes(raw.try_into().unwrap());
    if !plausible_u32_value(value, raw, path) {
        return;
    }
    let handle = format!("0x{value:08x}");
    push_candidate(
        path,
        candidates,
        kind,
        handle,
        value_offset,
        anchor_offset,
        hex::encode(raw),
        score,
    );
}

#[allow(clippy::too_many_arguments)]
fn push_candidate(
    path: &PathCandidate,
    candidates: &mut Vec<NetIdentityCandidate>,
    kind: NetIdentityCandidateKind,
    handle: String,
    byte_offset: usize,
    anchor_offset: usize,
    raw_hex: String,
    score: u16,
) {
    let relative_offset = byte_offset as isize - anchor_offset as isize;
    if candidates.iter().any(|candidate| {
        candidate.kind == kind
            && candidate.handle == handle
            && candidate.relative_offset == relative_offset
    }) {
        return;
    }
    candidates.push(NetIdentityCandidate {
        kind,
        handle,
        path: path.value.clone(),
        byte_offset,
        bit_shift: path.bit_shift,
        relative_offset,
        raw_hex,
        score,
    });
}

fn read_serialized_int_packed(data: &[u8], offset: usize) -> Option<(u32, usize)> {
    let mut value = 0_u32;
    for index in 0..5 {
        let byte = *data.get(offset + index)?;
        value |= u32::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

fn plausible_u32_value(value: u32, raw: &[u8], path: &PathCandidate) -> bool {
    if value == 0 || value == u32::MAX {
        return false;
    }
    if value == path.value.len() as u32 || value == (path.value.len() + 1) as u32 {
        return false;
    }
    if value < 0x80 {
        return false;
    }
    if raw[..3].iter().all(|byte| *byte == 0) {
        return false;
    }
    !raw.iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
}

fn plausible_packed_value(value: u32, raw: &[u8], path: &PathCandidate) -> bool {
    value >= 0x80
        && value != path.value.len() as u32
        && value != (path.value.len() + 1) as u32
        && value != u32::MAX
        && !raw
            .iter()
            .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
}

fn packed_score(offset: usize, anchor_offset: usize, width: usize) -> u16 {
    let distance = anchor_offset.abs_diff(offset) as u16;
    let base: u16 = if width > 1 { 46 } else { 28 };
    base.saturating_sub(distance.min(18))
}
