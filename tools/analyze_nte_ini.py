#!/usr/bin/env python3
"""Decrypt and summarize NTE line-encrypted INI files.

The keys and algorithm mirror CUE4Parse's public NTE INI implementation.
Input files are read-only. Potential account and device values are redacted
from the generated report.
"""

from __future__ import annotations

import argparse
import base64
import configparser
import json
import re
from pathlib import Path
from typing import Iterable

from Crypto.Cipher import AES
from Crypto.Util.Padding import unpad


KEYS = {
    "global": b"UVbP6pjjw5KZhvddie3tfhg1pVkkveY8",
    "china": b"1zh6IOlIohrR88UNPjiLisrkWACUQYuz",
}

SENSITIVE = re.compile(
    r"(account|token|password|passwd|secret|cookie|session|openid|user.?id|"
    r"device|uuid|guid|login|contact|realname|nickname)",
    re.IGNORECASE,
)

INTERESTING = re.compile(
    r"(aes|crypto|encrypt|pak|iostore|utoc|ucas|endpoint|host|server|url|"
    r"port|net|socket|udp|tcp|protocol|packet|log|debug|trace|version|branch|"
    r"build|environment|region|channel|cdn|patch)",
    re.IGNORECASE,
)


def decrypt_line(line: str) -> tuple[str, str] | None:
    try:
        encrypted = base64.b64decode(line, validate=True)
    except (ValueError, base64.binascii.Error):
        return None
    if not encrypted or len(encrypted) % AES.block_size:
        return None

    for name, key in KEYS.items():
        try:
            raw = unpad(AES.new(key, AES.MODE_ECB).decrypt(encrypted), AES.block_size)
            text = raw.decode("utf-8")
        except (ValueError, UnicodeDecodeError):
            continue
        return name, text
    return None


def decrypt_text(text: str) -> tuple[str, list[str], int]:
    active_key = "plaintext"
    output: list[str] = []
    encrypted_lines = 0
    for original in text.lstrip("\ufeff").splitlines():
        line = original.strip()
        if not line:
            continue
        decrypted = decrypt_line(line)
        if decrypted is None:
            output.append(original)
            continue
        key_name, value = decrypted
        active_key = key_name
        encrypted_lines += 1
        output.extend(part for part in value.split("|SPLIT|") if part)
    return active_key, output, encrypted_lines


def redact_line(line: str) -> str:
    if "=" not in line:
        return line
    key, value = line.split("=", 1)
    if SENSITIVE.search(key):
        return f"{key}=<redacted:{len(value)}>"
    return line


def parse_entries(lines: Iterable[str]) -> list[dict[str, str]]:
    parser = configparser.RawConfigParser(strict=False, interpolation=None)
    parser.optionxform = str
    text = "\n".join(lines)
    try:
        parser.read_string(text)
    except configparser.Error:
        return []

    entries = []
    for section in parser.sections():
        for key, value in parser.items(section):
            redacted = "<redacted>" if SENSITIVE.search(key) else value
            entries.append({"section": section, "key": key, "value": redacted})
    return entries


def analyze_file(path: Path, output_dir: Path) -> dict[str, object]:
    text = path.read_text(encoding="utf-8-sig", errors="replace")
    key_name, lines, encrypted_lines = decrypt_text(text)
    redacted_lines = [redact_line(line) for line in lines]
    entries = parse_entries(redacted_lines)
    interesting = [
        entry
        for entry in entries
        if INTERESTING.search(entry["section"])
        or INTERESTING.search(entry["key"])
        or INTERESTING.search(entry["value"])
    ]

    safe_name = re.sub(r"[^A-Za-z0-9_.-]+", "_", path.name)
    decrypted_path = output_dir / f"{safe_name}.decrypted.redacted.ini"
    decrypted_path.write_text("\n".join(redacted_lines) + "\n", encoding="utf-8")
    return {
        "path": str(path.resolve()),
        "key": key_name,
        "input_lines": len(text.splitlines()),
        "decrypted_lines": encrypted_lines,
        "parsed_entries": len(entries),
        "interesting_entries": interesting,
        "redacted_output": str(decrypted_path.resolve()),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="解析 NTE 加密 INI 配置")
    parser.add_argument("inputs", nargs="+", type=Path)
    parser.add_argument("--output", type=Path, default=Path("target/ini-analysis"))
    args = parser.parse_args()

    output_dir = args.output.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    reports = [analyze_file(path.resolve(), output_dir) for path in args.inputs]
    report_path = output_dir / "report.json"
    report_path.write_text(
        json.dumps({"files": reports}, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(report_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
