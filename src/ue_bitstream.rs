use std::collections::HashSet;

const MIN_CANDIDATE_LEN: usize = 4;
const MAX_CANDIDATE_LEN: usize = 160;
const MAX_PATH_CANDIDATES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCandidate {
    pub value: String,
    pub byte_offset: usize,
    pub bit_shift: u8,
    pub score: u16,
}

#[derive(Clone, Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_offset: usize,
}

impl<'a> BitReader<'a> {
    #[allow(dead_code)]
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_offset: 0,
        }
    }

    #[allow(dead_code)]
    pub fn with_offset(data: &'a [u8], bit_offset: usize) -> Option<Self> {
        (bit_offset <= data.len() * 8).then_some(Self { data, bit_offset })
    }

    #[allow(dead_code)]
    pub fn position(&self) -> usize {
        self.bit_offset
    }

    #[allow(dead_code)]
    pub fn read_bits_lsb(&mut self, bit_count: usize) -> Option<u64> {
        let value = read_bits_lsb(self.data, self.bit_offset, bit_count)?;
        self.bit_offset += bit_count;
        Some(value)
    }

    #[allow(dead_code)]
    pub fn read_u32_le(&mut self) -> Option<u32> {
        let mut bytes = [0; 4];
        self.read_bytes(&mut bytes)?;
        Some(u32::from_le_bytes(bytes))
    }

    #[allow(dead_code)]
    pub fn read_u64_le(&mut self) -> Option<u64> {
        let mut bytes = [0; 8];
        self.read_bytes(&mut bytes)?;
        Some(u64::from_le_bytes(bytes))
    }

    #[allow(dead_code)]
    pub fn read_f32_le(&mut self) -> Option<f32> {
        Some(f32::from_bits(self.read_u32_le()?))
    }

    #[allow(dead_code)]
    pub fn read_f64_le(&mut self) -> Option<f64> {
        Some(f64::from_bits(self.read_u64_le()?))
    }

    #[allow(dead_code)]
    pub fn read_bytes(&mut self, output: &mut [u8]) -> Option<()> {
        decode_shifted_into(
            self.data,
            self.bit_offset / 8,
            (self.bit_offset % 8) as u8,
            0,
            output,
        )?;
        self.bit_offset += output.len() * 8;
        Some(())
    }
}

#[allow(dead_code)]
pub fn read_bits_lsb(data: &[u8], bit_offset: usize, bit_count: usize) -> Option<u64> {
    if bit_count > 64 || bit_offset.checked_add(bit_count)? > data.len() * 8 {
        return None;
    }
    let mut value = 0_u64;
    for index in 0..bit_count {
        let source_bit = bit_offset + index;
        let bit = (data[source_bit / 8] >> (source_bit % 8)) & 1;
        value |= u64::from(bit) << index;
    }
    Some(value)
}

pub fn decode_shifted_into(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
    start_bit_offset: usize,
    output: &mut [u8],
) -> Option<()> {
    if bit_shift >= 8 {
        return None;
    }
    for (index, byte) in output.iter_mut().enumerate() {
        let bit_position = bit_shift as usize + start_bit_offset + index * 8;
        let source_offset = byte_offset + bit_position / 8;
        let source_shift = bit_position % 8;
        let current = *data.get(source_offset)?;
        let mut value = (current as u16) >> source_shift;
        if source_shift != 0 {
            value |= (*data.get(source_offset + 1)? as u16) << (8 - source_shift);
        }
        *byte = value as u8;
    }
    Some(())
}

pub fn decode_shifted_bytes(
    data: &[u8],
    byte_offset: usize,
    bit_shift: u8,
    start_bit_offset: usize,
    count: usize,
) -> Option<Vec<u8>> {
    let mut output = vec![0; count];
    decode_shifted_into(data, byte_offset, bit_shift, start_bit_offset, &mut output)?;
    Some(output)
}

#[allow(dead_code)]
pub fn read_fstring_like(data: &[u8], byte_offset: usize, bit_shift: u8) -> Option<String> {
    let mut header = [0; 4];
    decode_shifted_into(data, byte_offset, bit_shift, 0, &mut header)?;
    let length = i32::from_le_bytes(header);
    if length == 0 || length.unsigned_abs() as usize > MAX_CANDIDATE_LEN + 1 {
        return None;
    }
    if length < 0 {
        return None;
    }
    let length = length as usize;
    let mut raw = vec![0; length];
    decode_shifted_into(data, byte_offset, bit_shift, 32, &mut raw)?;
    let bytes = raw.strip_suffix(&[0]).unwrap_or(&raw);
    let value = std::str::from_utf8(bytes).ok()?.trim();
    (path_candidate_score(value) > 0).then(|| value.to_owned())
}

pub fn extract_path_candidates(data: &[u8]) -> Vec<PathCandidate> {
    let mut candidates = Vec::new();
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
        for (offset, value) in length_prefixed_candidates(&shifted) {
            push_candidate(&mut candidates, &mut seen, value, offset, bit_shift);
        }
        for (offset, bytes) in ascii_runs(&shifted) {
            let Ok(value) = std::str::from_utf8(bytes) else {
                continue;
            };
            for token in value.split_whitespace() {
                push_candidate(&mut candidates, &mut seen, token.trim(), offset, bit_shift);
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
    candidates.truncate(MAX_PATH_CANDIDATES);
    candidates
}

fn push_candidate(
    candidates: &mut Vec<PathCandidate>,
    seen: &mut HashSet<String>,
    value: &str,
    byte_offset: usize,
    bit_shift: u8,
) {
    let score = path_candidate_score(value);
    if score == 0 || !seen.insert(value.to_owned()) {
        return;
    }
    candidates.push(PathCandidate {
        value: value.to_owned(),
        byte_offset,
        bit_shift,
        score,
    });
}

fn length_prefixed_candidates(data: &[u8]) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    for offset in 0..data.len().saturating_sub(8) {
        let Some(length_bytes) = data.get(offset..offset + 4) else {
            continue;
        };
        let length = u32::from_le_bytes(length_bytes.try_into().unwrap()) as usize;
        if !(MIN_CANDIDATE_LEN + 1..=MAX_CANDIDATE_LEN + 1).contains(&length) {
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
        found.push((offset + 4, value.trim()));
    }
    found
}

fn ascii_runs(data: &[u8]) -> Vec<(usize, &[u8])> {
    let mut runs = Vec::new();
    let mut start = None;
    for (index, byte) in data.iter().enumerate() {
        if (0x20..=0x7e).contains(byte) {
            start.get_or_insert(index);
            continue;
        }
        if let Some(run_start) = start.take()
            && index - run_start >= MIN_CANDIDATE_LEN
        {
            runs.push((run_start, &data[run_start..index]));
        }
    }
    if let Some(run_start) = start
        && data.len() - run_start >= MIN_CANDIDATE_LEN
    {
        runs.push((run_start, &data[run_start..]));
    }
    runs
}

fn path_candidate_score(value: &str) -> u16 {
    let value = value.trim_matches('\0').trim();
    if !(MIN_CANDIDATE_LEN..=MAX_CANDIDATE_LEN).contains(&value.len()) {
        return 0;
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'/' | b'-')
    }) {
        return 0;
    }
    let lower = value.to_ascii_lowercase();
    let targetish = [
        "monster",
        "boss",
        "enemy",
        "npc",
        "mon_",
        "htcharacter",
        "character",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if value.starts_with("/Game/") && targetish {
        return 240;
    }
    if value.starts_with("/Game/") {
        return 180;
    }
    if targetish {
        return 140;
    }
    let has_structure = value.contains('/') || value.contains('.') || value.contains('_');
    let has_letters = value.bytes().filter(u8::is_ascii_alphabetic).count() >= 4;
    if has_structure && has_letters { 80 } else { 0 }
}
