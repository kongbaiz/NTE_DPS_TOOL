use aes::Aes256;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub(crate) enum EncryptedIniKey {
    #[default]
    Global,
    China,
}

impl EncryptedIniKey {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::China => "china",
        }
    }

    pub(crate) fn key(self) -> &'static [u8; 32] {
        match self {
            Self::Global => b"UVbP6pjjw5KZhvddie3tfhg1pVkkveY8",
            Self::China => b"1zh6IOlIohrR88UNPjiLisrkWACUQYuz",
        }
    }

    pub(crate) fn all() -> [Self; 2] {
        [Self::Global, Self::China]
    }
}

#[derive(Default)]
pub(crate) struct EncryptedIniRecord {
    pub(crate) encrypted_line: String,
    payload_parts: Vec<String>,
    visible_parts: Vec<String>,
}

pub(crate) fn encrypted_ini_search_matches(text: &str, query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    if query.is_ascii() {
        let query = query.as_bytes();
        let text_bytes = text.as_bytes();
        if query.len() > text_bytes.len() {
            return Vec::new();
        }
        return text_bytes
            .windows(query.len())
            .enumerate()
            .filter_map(|(index, window)| {
                (text.is_char_boundary(index) && window.eq_ignore_ascii_case(query))
                    .then_some(index)
            })
            .collect();
    }

    let lower_text = text.to_lowercase();
    let lower_query = query.to_lowercase();
    lower_text
        .match_indices(&lower_query)
        .filter_map(|(index, _)| text.is_char_boundary(index).then_some(index))
        .collect()
}

pub(crate) fn encrypted_ini_text_fingerprint(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(crate) fn parse_encrypted_ini_text(
    text: &str,
) -> Result<
    (
        EncryptedIniKey,
        String,
        Vec<EncryptedIniRecord>,
        String,
        bool,
    ),
    String,
> {
    let mut active_key = EncryptedIniKey::Global;
    let mut output = Vec::new();
    let mut records = Vec::new();
    let line_ending = if text.contains("\r\n") {
        "\r\n".to_owned()
    } else {
        "\n".to_owned()
    };
    let final_newline = text.ends_with('\n') || text.ends_with('\r');
    for original in text.trim_start_matches('\u{feff}').lines() {
        let line = original.trim();
        if line.is_empty() {
            records.push(EncryptedIniRecord {
                encrypted_line: original.to_owned(),
                payload_parts: Vec::new(),
                visible_parts: Vec::new(),
            });
            continue;
        }
        if let Some((key, decrypted)) = decrypt_encrypted_ini_line(line)? {
            active_key = key;
            let payload_parts = decrypted
                .split("|SPLIT|")
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let visible_parts = payload_parts
                .iter()
                .filter(|part| !part.is_empty())
                .cloned()
                .collect::<Vec<_>>();
            output.extend(visible_parts.iter().cloned());
            records.push(EncryptedIniRecord {
                encrypted_line: original.to_owned(),
                payload_parts,
                visible_parts,
            });
        } else {
            output.push(original.to_owned());
            records.push(EncryptedIniRecord {
                encrypted_line: original.to_owned(),
                payload_parts: vec![original.to_owned()],
                visible_parts: vec![original.to_owned()],
            });
        }
    }
    Ok((
        active_key,
        output.join("\n"),
        records,
        line_ending,
        final_newline,
    ))
}

#[cfg(test)]
pub(crate) fn decrypt_encrypted_ini_text(
    text: &str,
) -> Result<(EncryptedIniKey, String, usize), String> {
    let (key, plaintext, records, _, _) = parse_encrypted_ini_text(text)?;
    Ok((key, plaintext, records.len()))
}

fn decrypt_encrypted_ini_line(line: &str) -> Result<Option<(EncryptedIniKey, String)>, String> {
    let Ok(encrypted) = BASE64.decode(line) else {
        return Ok(None);
    };
    if encrypted.is_empty() || !encrypted.len().is_multiple_of(16) {
        return Ok(None);
    }
    for key in EncryptedIniKey::all() {
        let decrypted = decrypt_aes256_ecb(&encrypted, key.key())?;
        let Ok(unpadded) = pkcs7_unpad(&decrypted) else {
            continue;
        };
        let Ok(text) = String::from_utf8(unpadded.to_vec()) else {
            continue;
        };
        return Ok(Some((key, text)));
    }
    Ok(None)
}

#[cfg(test)]
pub(crate) fn encrypt_encrypted_ini_text(
    text: &str,
    key: EncryptedIniKey,
) -> Result<String, String> {
    let mut output = String::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let encrypted = encrypt_aes256_ecb(&pkcs7_pad(line.as_bytes()), key.key())?;
        output.push_str(&BASE64.encode(encrypted));
        output.push('\n');
    }
    Ok(output)
}

pub(crate) fn encrypt_encrypted_ini_records(
    text: &str,
    key: EncryptedIniKey,
    original_key: EncryptedIniKey,
    records: &[EncryptedIniRecord],
    line_ending: &str,
    final_newline: bool,
) -> Result<String, String> {
    let mut output_lines = Vec::new();
    let lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut line_index = 0;
    for record in records {
        let visible_count = record.visible_parts.len();
        if visible_count == 0 {
            output_lines.push(record.encrypted_line.clone());
            continue;
        }
        if line_index + visible_count > lines.len() {
            break;
        }
        let current_parts = &lines[line_index..line_index + visible_count];
        if key == original_key && current_parts == record.visible_parts.as_slice() {
            output_lines.push(record.encrypted_line.clone());
        } else {
            let mut payload_parts = record.payload_parts.clone();
            let mut current_index = 0;
            for part in &mut payload_parts {
                if !part.is_empty() {
                    *part = current_parts[current_index].clone();
                    current_index += 1;
                }
            }
            let payload = payload_parts.join("|SPLIT|");
            let encrypted = encrypt_aes256_ecb(&pkcs7_pad(payload.as_bytes()), key.key())?;
            let encrypted_line = BASE64.encode(encrypted);
            output_lines.push(encrypted_line);
        }
        line_index += visible_count;
    }
    for line in lines
        .iter()
        .skip(line_index)
        .filter(|line| !line.trim().is_empty())
    {
        let encrypted = encrypt_aes256_ecb(&pkcs7_pad(line.as_bytes()), key.key())?;
        let encrypted_line = BASE64.encode(encrypted);
        output_lines.push(encrypted_line);
    }
    let mut output = output_lines.join(line_ending);
    if final_newline {
        output.push_str(line_ending);
    }
    Ok(output)
}

fn decrypt_aes256_ecb(data: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    if !data.len().is_multiple_of(16) {
        return Err("AES 密文长度不是 16 字节块的整数倍".to_owned());
    }
    let cipher = Aes256::new_from_slice(key).map_err(|error| error.to_string())?;
    let mut output = data.to_vec();
    for block in output.chunks_exact_mut(16) {
        cipher.decrypt_block(block.into());
    }
    Ok(output)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn encrypt_aes256_ecb(data: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    if !data.len().is_multiple_of(16) {
        return Err("AES 明文长度不是 16 字节块的整数倍".to_owned());
    }
    let cipher = Aes256::new_from_slice(key).map_err(|error| error.to_string())?;
    let mut output = data.to_vec();
    for block in output.chunks_exact_mut(16) {
        cipher.encrypt_block(block.into());
    }
    Ok(output)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let padding = 16 - data.len() % 16;
    let mut output = Vec::with_capacity(data.len() + padding);
    output.extend_from_slice(data);
    output.extend(std::iter::repeat_n(padding as u8, padding));
    output
}

fn pkcs7_unpad(data: &[u8]) -> Result<&[u8], String> {
    let Some(&padding) = data.last() else {
        return Err("空数据无法移除 PKCS#7 padding".to_owned());
    };
    let padding = usize::from(padding);
    if padding == 0 || padding > 16 || padding > data.len() {
        return Err("PKCS#7 padding 无效".to_owned());
    }
    if !data[data.len() - padding..]
        .iter()
        .all(|byte| usize::from(*byte) == padding)
    {
        return Err("PKCS#7 padding 不一致".to_owned());
    }
    Ok(&data[..data.len() - padding])
}
