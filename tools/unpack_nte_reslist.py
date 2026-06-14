#!/usr/bin/env python3
"""Decrypt NTE PatcherSDK manifests without modifying the source files."""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
import xml.etree.ElementTree as ET
import zlib
from pathlib import Path

from Crypto.Cipher import AES


MAGIC = b"PatcherXML0\x00"
KEY = b"1289@Patcher0000"
IV = b"PatcherSDK000000"
MANIFEST_NAMES = {
    "reslist.bin",
    "reslist.xml",
    "lastdiff.bin",
    "lastdiff.xml",
}


def find_inputs(paths: list[Path]) -> list[Path]:
    found: set[Path] = set()
    for path in paths:
        path = path.resolve()
        if path.is_file():
            found.add(path)
        elif path.is_dir():
            found.update(
                item.resolve()
                for item in path.rglob("*")
                if item.is_file() and item.name.lower() in MANIFEST_NAMES
            )
        else:
            raise FileNotFoundError(path)
    return sorted(found)


def decrypt_manifest(path: Path) -> tuple[bytes, dict[str, object]]:
    data = path.read_bytes()
    if len(data) < 32 or data[:12] != MAGIC:
        raise ValueError("not a PatcherXML0 encrypted manifest")
    if (len(data) - 16) % AES.block_size:
        raise ValueError("encrypted payload is not AES block aligned")

    expected_size = struct.unpack_from("<I", data, 12)[0]
    decrypted = AES.new(KEY, AES.MODE_CBC, IV).decrypt(data[16:])
    xml_data = zlib.decompress(decrypted)
    if len(xml_data) != expected_size:
        raise ValueError(
            f"decompressed size mismatch: expected {expected_size}, got {len(xml_data)}"
        )

    root = ET.fromstring(xml_data)
    entries = [dict(item.attrib) for item in root]
    resource_entries = [item for item in entries if "filename" in item]
    patch_entries = [item for item in entries if "patch" in item]
    container_suffixes = (".pak", ".sig", ".ucas", ".utoc")
    container_entries = [
        item
        for item in resource_entries
        if item["filename"].lower().endswith(container_suffixes)
    ]
    summary: dict[str, object] = {
        "source": str(path),
        "source_size": len(data),
        "xml_size": len(xml_data),
        "xml_sha256": hashlib.sha256(xml_data).hexdigest(),
        "root": root.tag,
        "attributes": dict(root.attrib),
        "entry_count": len(entries),
        "resource_count": len(resource_entries),
        "patch_count": len(patch_entries),
        "container_count": len(container_entries),
        "containers": container_entries,
    }
    return xml_data, summary


def output_name(path: Path, used: set[str]) -> str:
    stem = path.stem
    name = f"{stem}.decrypted.xml"
    index = 2
    while name.lower() in used:
        name = f"{stem}.{index}.decrypted.xml"
        index += 1
    used.add(name.lower())
    return name


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Decrypt NTE ResList/lastdiff PatcherXML0 manifests."
    )
    parser.add_argument("inputs", nargs="+", type=Path, help="Manifest files or folders")
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("target/reslist-analysis"),
        help="Destination for decrypted XML and report.json",
    )
    args = parser.parse_args()

    inputs = find_inputs(args.inputs)
    if not inputs:
        parser.error("no ResList or lastdiff files found")

    args.output_dir.mkdir(parents=True, exist_ok=True)
    report: dict[str, object] = {"files": [], "errors": []}
    used_names: set[str] = set()
    for path in inputs:
        try:
            xml_data, summary = decrypt_manifest(path)
            destination = args.output_dir / output_name(path, used_names)
            destination.write_bytes(xml_data)
            summary["output"] = str(destination.resolve())
            report["files"].append(summary)
            print(
                f"OK {path}: {summary['root']} entries={summary['entry_count']} "
                f"containers={summary['container_count']}"
            )
        except (OSError, ValueError, zlib.error, ET.ParseError) as error:
            report["errors"].append({"source": str(path), "error": str(error)})
            print(f"ERROR {path}: {error}")

    report_path = args.output_dir / "report.json"
    report_path.write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(f"Report: {report_path.resolve()}")
    return 1 if report["errors"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
