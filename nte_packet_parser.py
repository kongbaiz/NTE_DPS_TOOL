#!/usr/bin/env python3
"""NTE damage packet parser with offline and live capture modes."""

from __future__ import annotations

import argparse
import ipaddress
import json
import math
import re
import struct
import sys
import time
from collections import Counter, defaultdict
from dataclasses import asdict, dataclass, replace
from datetime import datetime
from pathlib import Path
from typing import Any, Iterable, Iterator, TextIO


BASE_DIR = Path(__file__).resolve().parent
FIELD_NAMES = tuple(f"F{i}" for i in range(1, 10))
HEX_RE = re.compile(r"^[0-9a-fA-F\s:-]+$")
CHAR_DECLARATION_RE = re.compile(rb"\x05\x00\x00\x00(\d{4})\x00")
MIN_DAMAGE = 2.0
MAX_DAMAGE = 1_000_000_000.0
MAX_FIELD_LENGTH = 64
SCAN_START = 200
SCAN_END = 700
FLOAT_FIELD_SEPARATOR = 12
DOUBLE_FIELD_SEPARATOR = 13
INT_FIELD_SEPARATOR = 6
MIN_CHARACTER_ID = 1000
MAX_CHARACTER_ID = 9999


def load_characters(path: Path = BASE_DIR / "characters.json") -> dict[int, str]:
    document = json.loads(path.read_text(encoding="utf-8"))
    return {
        int(char_id): row.get("name_zh") or row.get("name_en") or char_id
        for char_id, row in document["characters"].items()
    }


CHARACTERS = load_characters()


def character_name(char_id: int) -> str:
    return CHARACTERS.get(char_id, f"未知角色({char_id})")


@dataclass(frozen=True)
class Hit:
    timestamp: float
    char_id: int
    char_name: str
    char_known: bool
    damage: float
    byte_offset: int
    bit_shift: int
    char_source: str
    direction: str
    target_hp_before: float
    target_hp_after: float
    target_max_hp: float
    target_hp_percent: float
    fields: dict[str, dict[str, Any]]


def decode_shifted_bytes(
    data: bytes,
    byte_offset: int,
    bit_shift: int,
    start_bit_offset: int,
    count: int,
) -> bytes:
    output = bytearray()
    for index in range(count):
        bit_position = bit_shift + start_bit_offset + index * 8
        source_offset = byte_offset + bit_position // 8
        source_shift = bit_position % 8
        if source_offset >= len(data):
            raise IndexError("bitstream ended before requested bytes")

        value = data[source_offset] >> source_shift
        if source_shift:
            if source_offset + 1 >= len(data):
                raise IndexError("bitstream ended inside shifted byte")
            value |= data[source_offset + 1] << (8 - source_shift)
        output.append(value & 0xFF)
    return bytes(output)


def read_field(
    data: bytes,
    byte_offset: int,
    bit_shift: int,
    bit_offset: int,
) -> tuple[int, bytes, int] | None:
    try:
        header = decode_shifted_bytes(data, byte_offset, bit_shift, bit_offset, 5)
    except IndexError:
        return None

    separator = header[0]
    field_length = int.from_bytes(header[1:5], "little")
    consumed_bits = 40 + field_length * 8
    remaining_bits = (len(data) - byte_offset) * 8
    if (
        field_length == 0
        or field_length > MAX_FIELD_LENGTH
        or bit_offset + consumed_bits > remaining_bits
    ):
        return None

    try:
        raw = decode_shifted_bytes(
            data, byte_offset, bit_shift, bit_offset + 40, field_length
        )
    except IndexError:
        return None
    return separator, raw, consumed_bits


def describe_field(raw: bytes) -> dict[str, Any]:
    result: dict[str, Any] = {"hex": raw.hex()}
    if len(raw) >= 4:
        result["i32"] = struct.unpack("<i", raw[:4])[0]
        f32 = struct.unpack("<f", raw[:4])[0]
        if math.isfinite(f32):
            result["f32"] = round(f32, 4)
    if len(raw) >= 8:
        f64 = struct.unpack("<d", raw[:8])[0]
        if math.isfinite(f64):
            result["f64"] = round(f64, 6)
    return result


def find_declared_character_evidence(
    data: bytes,
) -> list[tuple[int, int, int]]:
    found: list[tuple[int, int, int]] = []
    for bit_shift in range(8):
        if bit_shift == 0:
            shifted = data
        else:
            try:
                shifted = decode_shifted_bytes(
                    data, 0, bit_shift, 0, max(0, len(data) - 1)
                )
            except IndexError:
                continue
        for match in CHAR_DECLARATION_RE.finditer(shifted):
            char_id = int(match.group(1))
            evidence = (char_id, bit_shift, match.start())
            if (
                MIN_CHARACTER_ID <= char_id <= MAX_CHARACTER_ID
                and evidence not in found
            ):
                found.append(evidence)
    return found


def find_declared_character_ids(data: bytes) -> list[int]:
    return list(
        dict.fromkeys(
            char_id
            for char_id, _bit_shift, _offset in (
                find_declared_character_evidence(data)
            )
        )
    )


def find_character_id(data: bytes, hit_offset: int) -> int | None:
    declarations = find_declared_character_ids(data)
    if len(declarations) == 1:
        return declarations[0]
    return None


def parse_damage_payload(
    data: bytes,
    timestamp: float | None = None,
    allow_unknown: bool = False,
    fallback_char_id: int | None = None,
    packet_char_id: int | None = None,
    character_evidence: list[tuple[int, int, int]] | None = None,
) -> list[Hit]:
    hits: list[Hit] = []
    seen: set[tuple[int, int, int, int]] = set()
    end_offset = min(len(data), SCAN_END)
    if packet_char_id is None:
        packet_char_id = find_character_id(data, 0)
    if character_evidence is None:
        character_evidence = find_declared_character_evidence(data)

    for byte_offset in range(SCAN_START, end_offset):
        for bit_shift in range(8):
            try:
                f0 = decode_shifted_bytes(data, byte_offset, bit_shift, 0, 4)
                damage = struct.unpack("<f", f0)[0]
            except (IndexError, struct.error):
                continue
            if not math.isfinite(damage) or not MIN_DAMAGE <= damage <= MAX_DAMAGE:
                continue

            fields: dict[str, dict[str, Any]] = {}
            bit_cursor = 32
            for field_name in FIELD_NAMES:
                parsed = read_field(data, byte_offset, bit_shift, bit_cursor)
                if parsed is None:
                    break
                separator, raw, consumed = parsed
                description = describe_field(raw)
                description["separator"] = separator
                fields[field_name] = description
                bit_cursor += consumed

            if len(fields) != len(FIELD_NAMES):
                continue
            if (
                fields["F1"]["separator"] != FLOAT_FIELD_SEPARATOR
                or fields["F2"]["separator"] != FLOAT_FIELD_SEPARATOR
                or fields["F3"]["separator"] != DOUBLE_FIELD_SEPARATOR
                or fields["F4"]["separator"] != FLOAT_FIELD_SEPARATOR
                or fields["F5"]["separator"] != FLOAT_FIELD_SEPARATOR
                or any(
                    fields[field_name]["separator"] != INT_FIELD_SEPARATOR
                    for field_name in ("F6", "F7", "F8")
                )
                or fields["F9"]["separator"] != FLOAT_FIELD_SEPARATOR
            ):
                continue

            f5_damage = fields["F5"].get("f32")
            if f5_damage is None or not math.isclose(
                damage,
                f5_damage,
                rel_tol=1e-6,
                abs_tol=0.01,
            ):
                continue

            char_id = packet_char_id or fallback_char_id
            if char_id is None and not allow_unknown:
                continue
            char_id = char_id or 0
            target_hp_before = float(fields["F1"]["f32"])
            target_max_hp = float(fields["F2"]["f32"])
            target_hp_after = max(0.0, target_hp_before - damage)
            target_hp_percent = (
                target_hp_after / target_max_hp * 100.0
                if target_max_hp > 0
                else 0.0
            )
            direction = (
                "incoming"
                if packet_char_id
                and any(
                    evidence_char_id == packet_char_id
                    and evidence_bit_shift == bit_shift
                    for (
                        evidence_char_id,
                        evidence_bit_shift,
                        _evidence_offset,
                    ) in character_evidence
                )
                else "outgoing"
                if packet_char_id
                else "unknown"
            )
            key = (char_id, round(damage), byte_offset, bit_shift)
            if key in seen:
                continue
            seen.add(key)
            hits.append(
                Hit(
                    timestamp=time.time() if timestamp is None else timestamp,
                    char_id=char_id,
                    char_name=character_name(char_id),
                    char_known=char_id in CHARACTERS,
                    damage=round(damage, 2),
                    byte_offset=byte_offset,
                    bit_shift=bit_shift,
                    char_source=(
                        "packet"
                        if packet_char_id
                        else "session"
                        if fallback_char_id
                        else "unknown"
                    ),
                    direction=direction,
                    target_hp_before=round(target_hp_before, 2),
                    target_hp_after=round(target_hp_after, 2),
                    target_max_hp=round(target_max_hp, 2),
                    target_hp_percent=round(target_hp_percent, 3),
                    fields=fields,
                )
            )
    return hits


def parse_hex(text: str) -> bytes:
    compact = re.sub(r"[\s:-]", "", text)
    if not compact or len(compact) % 2 or not HEX_RE.fullmatch(text):
        raise ValueError("无效的十六进制载荷")
    return bytes.fromhex(compact)


def iter_json_payloads(value: Any) -> Iterator[bytes]:
    if isinstance(value, dict):
        for key, child in value.items():
            if (
                key.lower() in {"data", "payload", "raw", "hex", "load"}
                and isinstance(child, str)
            ):
                try:
                    yield parse_hex(child)
                except ValueError:
                    pass
            else:
                yield from iter_json_payloads(child)
    elif isinstance(value, list):
        for child in value:
            yield from iter_json_payloads(child)


def require_scapy() -> tuple[Any, Any, Any, Any, Any, Any]:
    try:
        from scapy.all import IP, UDP, Raw, conf, rdpcap, sniff
    except ImportError as exc:
        raise RuntimeError(
            "抓包功能需要 Scapy，请运行: python -m pip install scapy"
        ) from exc
    return IP, UDP, Raw, conf, rdpcap, sniff


def _guess_local_ip(packets: Iterable[Any], ip_layer: Any) -> str | None:
    candidates: Counter[str] = Counter()
    for packet in packets:
        if ip_layer not in packet:
            continue
        source = packet[ip_layer].src
        try:
            if ipaddress.ip_address(source).is_private:
                candidates[source] += 1
        except ValueError:
            pass
    return candidates.most_common(1)[0][0] if candidates else None


def get_live_local_ip(conf: Any, interface: str | None) -> str:
    if interface:
        from scapy.all import get_if_addr

        local_ip = get_if_addr(interface)
    else:
        _iface, local_ip, _gateway = conf.route.route("8.8.8.8")
    if not local_ip or local_ip == "0.0.0.0":
        raise RuntimeError("无法自动识别本机 IP，请使用 --local-ip 指定")
    return local_ip


def iter_pcap_payloads(
    path: Path, local_ip: str | None
) -> Iterator[tuple[float, bytes]]:
    IP, _UDP, Raw, _conf, rdpcap, _sniff = require_scapy()
    packets = rdpcap(str(path))
    if local_ip is None:
        local_ip = _guess_local_ip(packets, IP)
        if local_ip:
            print(f"自动识别本机 IP: {local_ip}", file=sys.stderr)

    for packet in packets:
        if Raw not in packet or IP not in packet:
            continue
        if local_ip and packet[IP].src != local_ip:
            continue
        yield float(packet.time), bytes(packet[Raw].load)


def aggregate(hits: Iterable[Hit]) -> dict[int, dict[str, Any]]:
    stats: dict[int, dict[str, Any]] = defaultdict(
        lambda: {"name": "", "hits": 0, "damage": 0.0}
    )
    for hit in hits:
        row = stats[hit.char_id]
        row["name"] = hit.char_name
        row["hits"] += 1
        row["damage"] += hit.damage
    return dict(stats)


def format_hit(hit: Hit) -> str:
    event_time = datetime.fromtimestamp(hit.timestamp).astimezone()
    return (
        f"[{event_time:%Y-%m-%d %H:%M:%S.%f}] "
        f"角色={hit.char_name} ID={hit.char_id} 伤害={hit.damage:.2f} "
        f"目标HP={hit.target_hp_after:.2f}/{hit.target_max_hp:.2f} "
        f"({hit.target_hp_percent:.2f}%) 方向={hit.direction} "
        f"偏移={hit.byte_offset} "
        f"位移={hit.bit_shift} 来源={hit.char_source}"
    )


def print_report(hits: list[Hit], capture_mode: bool) -> None:
    duration = (
        max(hits[-1].timestamp - hits[0].timestamp, 0.001)
        if capture_mode and len(hits) > 1
        else 0.0
    )
    print(f"{'角色':<16} {'ID':>6} {'命中':>8} {'总伤害':>14} {'DPS':>14}")
    print("-" * 68)
    for char_id, row in sorted(
        aggregate(hits).items(),
        key=lambda item: item[1]["damage"],
        reverse=True,
    ):
        dps = row["damage"] / duration if duration else 0.0
        print(
            f"{row['name']:<16} {char_id:>6} {row['hits']:>8} "
            f"{row['damage']:>14.2f} {dps:>14.2f}"
        )
    print("-" * 68)
    print(f"命中记录: {len(hits)}，时间跨度: {duration:.3f}s")


class EventWriter:
    def __init__(self, path: Path | None) -> None:
        self.path = path
        self.handle: TextIO | None = None

    def __enter__(self) -> EventWriter:
        if self.path:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            self.handle = self.path.open("a", encoding="utf-8", newline="\n")
        return self

    def write(self, hit: Hit) -> None:
        print(format_hit(hit), flush=True)
        if self.handle:
            record = asdict(hit)
            record["time"] = datetime.fromtimestamp(
                hit.timestamp
            ).astimezone().isoformat()
            self.handle.write(json.dumps(record, ensure_ascii=False) + "\n")
            self.handle.flush()

    def __exit__(self, *_args: object) -> None:
        if self.handle:
            self.handle.close()


class RawPacketWriter:
    def __init__(self, path: Path, ip_layer: Any, udp_layer: Any) -> None:
        self.path = path
        self.ip_layer = ip_layer
        self.udp_layer = udp_layer
        self.handle: TextIO | None = None

    def __enter__(self) -> RawPacketWriter:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.handle = self.path.open("a", encoding="utf-8", newline="\n")
        return self

    def write(self, packet: Any, payload: bytes, hits: list[Hit]) -> None:
        ip = packet[self.ip_layer]
        transport = packet[self.udp_layer] if self.udp_layer in packet else None
        timestamp = float(packet.time)
        record = {
            "timestamp": timestamp,
            "time": datetime.fromtimestamp(timestamp).astimezone().isoformat(),
            "src_ip": ip.src,
            "dst_ip": ip.dst,
            "src_port": transport.sport if transport is not None else None,
            "dst_port": transport.dport if transport is not None else None,
            "protocol": "UDP" if transport is not None else str(ip.proto),
            "packet_length": len(bytes(packet)),
            "payload_length": len(payload),
            "packet_hex": bytes(packet).hex(),
            "payload_hex": payload.hex(),
            "declared_character_ids": find_declared_character_ids(payload),
            "parsed_hits": [
                {
                    "char_id": hit.char_id,
                    "char_name": hit.char_name,
                    "char_known": hit.char_known,
                    "damage": hit.damage,
                    "byte_offset": hit.byte_offset,
                    "bit_shift": hit.bit_shift,
                    "char_source": hit.char_source,
                    "direction": hit.direction,
                    "target_hp_before": hit.target_hp_before,
                    "target_hp_after": hit.target_hp_after,
                    "target_max_hp": hit.target_max_hp,
                    "target_hp_percent": hit.target_hp_percent,
                    "fields": hit.fields,
                }
                for hit in hits
            ],
        }
        assert self.handle is not None
        self.handle.write(json.dumps(record, ensure_ascii=False) + "\n")
        self.handle.flush()

    def __exit__(self, *_args: object) -> None:
        if self.handle:
            self.handle.close()


def default_live_outputs() -> tuple[Path, Path]:
    stamp = datetime.now().astimezone().strftime("%Y%m%d_%H%M%S")
    log_dir = BASE_DIR / "logs"
    return (
        log_dir / f"nte_hits_{stamp}.jsonl",
        log_dir / f"nte_raw_packets_{stamp}.jsonl",
    )


def run_live_capture(args: argparse.Namespace) -> list[Hit]:
    IP, UDP, Raw, conf, _rdpcap, sniff = require_scapy()
    local_ip = args.local_ip or get_live_local_ip(conf, args.interface)
    default_events, default_raw_packets = default_live_outputs()
    output = args.events_out or default_events
    raw_packets_output = args.raw_packets_out or default_raw_packets
    hits: list[Hit] = []
    session_characters: dict[tuple[str, int, str, int], int] = {}
    pending_packets: dict[
        tuple[str, int, str, int],
        list[tuple[Any, bytes, list[Hit], int | None]],
    ] = defaultdict(list)
    learned_damage_owners: dict[
        tuple[str, int, str, int], dict[float, set[int]]
    ] = defaultdict(lambda: defaultdict(set))
    observed_characters: set[int] = set()

    print(f"实时抓包已启动，本机 IP: {local_ip}")
    print(f"抓包网卡: {args.interface or 'Scapy 默认网卡'}")
    print(f"抓包过滤器: {args.capture_filter}")
    print(f"事件文件: {output.resolve()}")
    print(f"原始封包文件: {raw_packets_output.resolve()}")
    print("按 Ctrl+C 停止。\n")

    with (
        EventWriter(output) as writer,
        RawPacketWriter(raw_packets_output, IP, UDP) as raw_writer,
    ):
        def emit_packet(
            packet: Any,
            payload: bytes,
            packet_hits: list[Hit],
            write_raw: bool = True,
        ) -> None:
            if not packet_hits:
                return
            if write_raw:
                raw_writer.write(packet, payload, packet_hits)
            for hit in packet_hits:
                hits.append(hit)
                writer.write(hit)

        def assign_character(
            packet_hits: list[Hit],
            char_id: int,
            source: str,
        ) -> list[Hit]:
            return [
                replace(
                    hit,
                    char_id=char_id,
                    char_name=character_name(char_id),
                    char_known=char_id in CHARACTERS,
                    char_source=source,
                )
                if hit.char_id == 0
                else hit
                for hit in packet_hits
            ]

        def resolve_pending(
            session_key: tuple[str, int, str, int],
            next_char_id: int | None,
            flush_all: bool = False,
        ) -> None:
            remaining: list[tuple[Any, bytes, list[Hit], int | None]] = []
            for (
                pending_packet,
                pending_payload,
                pending_hits,
                previous_char_id,
            ) in pending_packets[session_key]:
                owners = {
                    owner
                    for hit in pending_hits
                    for owner in learned_damage_owners[session_key].get(
                        hit.damage, set()
                    )
                }
                resolved_char_id: int | None = None
                source = "unknown"
                if (
                    previous_char_id is not None
                    and previous_char_id == next_char_id
                ):
                    resolved_char_id = previous_char_id
                    source = "neighbor"
                elif len(owners) == 1:
                    resolved_char_id = owners.pop()
                    source = "learned"

                if resolved_char_id is not None:
                    emit_packet(
                        pending_packet,
                        pending_payload,
                        assign_character(
                            pending_hits,
                            resolved_char_id,
                            source,
                        ),
                    )
                elif flush_all:
                    raw_writer.write(
                        pending_packet,
                        pending_payload,
                        pending_hits,
                    )
                    if args.allow_unknown:
                        for hit in pending_hits:
                            hits.append(hit)
                            writer.write(hit)
                else:
                    remaining.append(
                        (
                            pending_packet,
                            pending_payload,
                            pending_hits,
                            (
                                previous_char_id
                                if next_char_id is None
                                else None
                            ),
                        )
                    )
            pending_packets[session_key] = remaining

        def handle_packet(packet: Any) -> None:
            if IP not in packet or Raw not in packet:
                return
            if packet[IP].src != local_ip:
                return
            payload = bytes(packet[Raw].load)
            transport = packet[UDP] if UDP in packet else None
            session_key = (
                packet[IP].src,
                transport.sport if transport is not None else 0,
                packet[IP].dst,
                transport.dport if transport is not None else 0,
            )
            character_evidence = find_declared_character_evidence(payload)
            declared_ids = list(
                dict.fromkeys(
                    char_id
                    for char_id, _bit_shift, _offset in character_evidence
                )
            )
            unique_declared_ids = list(dict.fromkeys(declared_ids))
            packet_char_id = (
                unique_declared_ids[0]
                if len(unique_declared_ids) == 1
                else None
            )
            if packet_char_id is not None:
                previous_char_id = session_characters.get(session_key)
                if packet_char_id not in observed_characters:
                    observed_characters.add(packet_char_id)
                    print(
                        f"发现伤害角色: {character_name(packet_char_id)} "
                        f"({packet_char_id})",
                        flush=True,
                    )
                resolve_pending(session_key, packet_char_id)
                session_characters[session_key] = packet_char_id

            packet_hits = parse_damage_payload(
                payload,
                timestamp=float(packet.time),
                allow_unknown=True,
                packet_char_id=packet_char_id,
                character_evidence=character_evidence,
            )
            if not packet_hits:
                return
            output_hits = [
                hit
                for hit in packet_hits
                if hit.direction != "incoming" or args.include_incoming
            ]
            if not output_hits:
                raw_writer.write(packet, payload, packet_hits)
                return
            if packet_char_id is not None:
                for hit in output_hits:
                    learned_damage_owners[session_key][hit.damage].add(
                        packet_char_id
                    )
                if len(output_hits) != len(packet_hits):
                    raw_writer.write(packet, payload, packet_hits)
                emit_packet(
                    packet,
                    payload,
                    output_hits,
                    write_raw=len(output_hits) == len(packet_hits),
                )
            else:
                pending = pending_packets[session_key]
                pending.append(
                    (
                        packet,
                        payload,
                        output_hits,
                        session_characters.get(session_key),
                    )
                )
                if len(pending) > 256:
                    old_packet, old_payload, old_hits, _old_char = pending.pop(0)
                    raw_writer.write(old_packet, old_payload, old_hits)
                    if args.allow_unknown:
                        for hit in old_hits:
                            hits.append(hit)
                            writer.write(hit)

        try:
            sniff(
                iface=args.interface,
                filter=args.capture_filter,
                prn=handle_packet,
                store=False,
            )
        except KeyboardInterrupt:
            pass
        except Exception as exc:
            raise RuntimeError(
                "实时抓包失败。请确认已安装 Npcap、终端权限足够，"
                "并检查 --interface 与 --capture-filter 参数。"
            ) from exc

        for session_key in list(pending_packets):
            resolve_pending(session_key, None, flush_all=True)

    hits.sort(key=lambda hit: hit.timestamp)
    print()
    print_report(hits, capture_mode=True)
    return hits


def make_synthetic_payload(
    char_id: int = 1033, damage: float = 12345.5
) -> bytes:
    data = bytearray(512)
    encoded_id = str(char_id).encode("ascii")
    data[10:19] = b"\x05\x00\x00\x00" + encoded_id + b"\x00"
    cursor = SCAN_START
    data[cursor : cursor + 4] = struct.pack("<f", damage)
    cursor += 4
    separators = (12, 12, 13, 12, 12, 6, 6, 6, 12)
    for index, separator in enumerate(separators, start=1):
        if index == 3:
            raw = struct.pack("<d", 12345.0)
        elif index == 5:
            raw = struct.pack("<f", damage)
        else:
            raw = struct.pack("<f", float(index))
        data[cursor] = separator
        data[cursor + 1 : cursor + 5] = len(raw).to_bytes(4, "little")
        data[cursor + 5 : cursor + 5 + len(raw)] = raw
        cursor += 5 + len(raw)
    return bytes(data)


def run_self_test() -> list[Hit]:
    direct_hits = parse_damage_payload(make_synthetic_payload())
    if len(direct_hits) != 1 or direct_hits[0].char_source != "packet":
        raise RuntimeError("内置测试失败：无法直接识别角色")

    payload_without_id = bytearray(
        make_synthetic_payload(char_id=1004, damage=321.0)
    )
    payload_without_id[10:14] = b"\x00\x00\x00\x00"
    session_hits = parse_damage_payload(
        bytes(payload_without_id),
        fallback_char_id=1010,
    )
    if (
        len(session_hits) != 1
        or session_hits[0].char_id != 1010
        or session_hits[0].char_source != "session"
    ):
        raise RuntimeError("内置测试失败：角色切换上下文未生效")

    shifted_payload = make_synthetic_payload(char_id=1004, damage=654.0)
    shifted_data = bytes(
        (
            (shifted_payload[index] << 3)
            | (
                shifted_payload[index - 1] >> 5
                if index > 0
                else 0
            )
        )
        & 0xFF
        for index in range(len(shifted_payload))
    )
    if find_declared_character_ids(shifted_data) != [1004]:
        raise RuntimeError("内置测试失败：无法识别位偏移角色 ID")
    return direct_hits


def build_argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="NTE 实时抓包与离线伤害数据解析器"
    )
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--live", action="store_true", help="实时抓包并解析")
    source.add_argument("--hex", dest="hex_payload", help="解析十六进制载荷")
    source.add_argument("--json", type=Path, help="解析诊断 JSON 中的载荷")
    source.add_argument("--pcap", type=Path, help="解析 PCAP/PCAPNG 文件")
    source.add_argument("--self-test", action="store_true", help="运行内置测试")
    parser.add_argument("--interface", help="实时抓包网卡名称或 GUID")
    parser.add_argument(
        "--capture-filter",
        default="udp",
        help="Scapy/libpcap 抓包过滤器，默认: udp",
    )
    parser.add_argument("--local-ip", help="只解析该本机源 IP 发出的数据")
    parser.add_argument(
        "--allow-unknown",
        action="store_true",
        help="保留封包中完全没有角色 ID 的完整 F0-F9 候选",
    )
    parser.add_argument(
        "--include-incoming",
        action="store_true",
        help="同时把角色受击伤害写入事件日志和统计",
    )
    parser.add_argument(
        "--events-out",
        type=Path,
        help="JSONL 事件文件；实时模式默认写入 logs 目录",
    )
    parser.add_argument(
        "--raw-packets-out",
        type=Path,
        help="成功解析封包的 JSONL 文件；实时模式默认写入 logs 目录",
    )
    return parser


def main() -> int:
    args = build_argument_parser().parse_args()
    if args.live:
        hits = run_live_capture(args)
        return 0 if hits else 2

    if args.self_test:
        hits = run_self_test()
        print_report(hits, capture_mode=False)
        return 0
    elif args.hex_payload:
        payloads = [(None, parse_hex(args.hex_payload))]
    elif args.json:
        document = json.loads(args.json.read_text(encoding="utf-8"))
        payloads = ((None, payload) for payload in iter_json_payloads(document))
    else:
        payloads = iter_pcap_payloads(args.pcap, args.local_ip)

    hits: list[Hit] = []
    with EventWriter(args.events_out) as writer:
        for packet_time, payload in payloads:
            for hit in parse_damage_payload(
                payload,
                timestamp=packet_time,
                allow_unknown=args.allow_unknown,
            ):
                hits.append(hit)
                if args.events_out:
                    writer.write(hit)

    hits.sort(key=lambda hit: hit.timestamp)
    print_report(hits, capture_mode=bool(args.pcap))
    return 0 if hits else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, RuntimeError, json.JSONDecodeError) as exc:
        print(f"错误: {exc}", file=sys.stderr)
        raise SystemExit(1)
