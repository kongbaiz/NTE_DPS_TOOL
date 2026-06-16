#!/usr/bin/env python3
"""Export selected NTE DataTables directly from game containers into res format."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import nte_asset_pipeline as pipeline


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
TABLE_GROUPS = {
    "gameplay-effect-mapping": ["gameplay_effect_mapping"],
    "skill-damage": ["skill_damage"],
    "wooden-descriptions": ["wooden_damage_descriptions"],
    "characters": ["characters"],
    "ability-tips": ["ability_tips"],
    "reactions": [
        "reaction_data",
        "reaction_detail",
        "reaction_damage",
        "reaction_elements",
        "reaction_extensions",
    ],
}
DEFAULT_TABLES = list(TABLE_GROUPS)
AES_KEY_RE = re.compile(r"^(?:0x)?[0-9a-fA-F]{64}$")


class ExportError(RuntimeError):
    pass


def selected_tables(groups: list[str] | None) -> list[str]:
    requested = groups or DEFAULT_TABLES
    result: list[str] = []
    for group in requested:
        if group == "all":
            group = ""
            names = [
                table
                for default_group in DEFAULT_TABLES
                for table in TABLE_GROUPS[default_group]
            ]
        else:
            names = TABLE_GROUPS[group]
        for name in names:
            if name not in result:
                result.append(name)
    return result


def resolve_key_file(
    explicit_path: Path | None, temporary_directory: Path
) -> tuple[Path, bool]:
    if explicit_path is not None:
        path = explicit_path.resolve()
        if not path.is_file():
            raise ExportError(f"AES key 文件不存在: {path}")
        key = path.read_text(encoding="utf-8").strip()
        if not AES_KEY_RE.fullmatch(key):
            raise ExportError("AES key 文件必须只包含一个 32 字节十六进制 key")
        return path, False

    key = os.environ.get("NTE_AES_KEY", "").strip()
    if not key:
        raise ExportError(
            "请使用 --aes-key-file 指定密钥文件，或设置 NTE_AES_KEY 环境变量"
        )
    if not AES_KEY_RE.fullmatch(key):
        raise ExportError("NTE_AES_KEY 必须是一个 32 字节十六进制 key")

    temporary_directory.mkdir(parents=True, exist_ok=True)
    descriptor, filename = tempfile.mkstemp(
        prefix=".nte-aes-", suffix=".tmp", dir=temporary_directory
    )
    os.close(descriptor)
    path = Path(filename)
    path.write_text(key, encoding="ascii")
    return path, True


def run_probe(args: argparse.Namespace, tables: list[str], raw_root: Path) -> dict[str, Any]:
    dotnet = args.dotnet.resolve()
    probe = args.probe.resolve()
    paks = args.paks_dir.resolve()
    paths_to_check = (
        ("dotnet", dotnet, False),
        ("CUE4Parse probe", probe, False),
        ("Paks 目录", paks, True),
    )
    if args.usmap is not None:
        usmap = args.usmap.resolve()
        paths_to_check = (*paths_to_check, ("usmap", usmap, False))

    for label, path, is_directory in paths_to_check:
        valid = path.is_dir() if is_directory else path.is_file()
        if not valid:
            raise ExportError(f"{label}不存在: {path}")

    key_file, remove_key_file = resolve_key_file(args.aes_key_file, raw_root)
    command = [
        str(dotnet),
        str(probe),
        "--paks",
        str(paks),
        "--output",
        str(raw_root),
        "--aes-key-file",
        str(key_file),
    ]
    if args.usmap is not None:
        command.extend(["--usmap", str(args.usmap.resolve())])
    for table in tables:
        command.extend(["--target", pipeline.REQUIRED_TABLES[table].removesuffix(".json")])

    try:
        result = subprocess.run(command, check=False)
    finally:
        if remove_key_file:
            key_file.unlink(missing_ok=True)
    if result.returncode:
        raise ExportError(f"CUE4Parse 导出失败，退出码 {result.returncode}")

    report_path = raw_root / "cue4parse_report.json"
    report = pipeline.read_json(report_path)
    if not isinstance(report, dict):
        raise ExportError(f"CUE4Parse 报告格式无效: {report_path}")
    failed = [
        target
        for target in report.get("targets", [])
        if target.get("status") != "exported"
    ]
    if failed:
        details = "; ".join(
            f"{target.get('target')}: {target.get('status')} "
            f"{target.get('error', '')}".strip()
            for target in failed
        )
        raise ExportError(f"部分资源导出失败: {details}")
    return report

def transform_tables(
    args: argparse.Namespace,
    tables: list[str],
    raw_root: Path,
    output_res: Path,
) -> dict[str, Any]:
    paths = {
        name: raw_root / pipeline.REQUIRED_TABLES[name]
        for name in tables
    }
    rows = {name: pipeline.table_rows(path) for name, path in paths.items()}
    outputs: dict[str, str] = {}
    counts = {name: len(table_rows) for name, table_rows in rows.items()}

    for name in pipeline.PROGRAM_TABLE_OUTPUTS:
        if name not in paths:
            continue
        relative = pipeline.PROGRAM_TABLE_OUTPUTS[name]
        destination = output_res / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(paths[name], destination)
        outputs[name] = relative

    if "characters" in rows:
        existing = pipeline.load_existing_characters(args.existing_res.resolve())
        _, character_report = pipeline.build_characters(
            raw_root,
            output_res,
            rows["characters"],
            existing,
            args.prune_stale_characters,
        )
        outputs["characters"] = "data/characters/characters.json"
        counts["character_output"] = character_report["output_rows"]

    if "ability_tips" in rows:
        destination = output_res / "data/skills/ability_tips.json"
        pipeline.write_json(destination, pipeline.build_ability_index(rows["ability_tips"]))
        outputs["ability_tips"] = "data/skills/ability_tips.json"

    reaction_tables = set(TABLE_GROUPS["reactions"])
    if reaction_tables.issubset(rows):
        destination = output_res / "data/reactions/reactions.json"
        pipeline.write_json(destination, pipeline.build_reaction_index(rows))
        outputs["reactions"] = "data/reactions/reactions.json"

    return {"counts": counts, "outputs": outputs}


def main() -> int:
    parser = argparse.ArgumentParser(
        description="使用 CUE4Parse 从 NTE 客户端直接生成项目 res 数据"
    )
    parser.add_argument("--paks-dir", type=Path, required=True)
    parser.add_argument(
        "--usmap",
        type=Path,
        default=None,
        help="可选：需要类型映射时指定 usmap 文件",
    )
    parser.add_argument("--aes-key-file", type=Path)
    parser.add_argument(
        "--table",
        action="append",
        choices=[*TABLE_GROUPS, "all"],
        help="可重复指定；默认导出全部项目数据表",
    )
    parser.add_argument("--output-res", type=Path, default=REPOSITORY_ROOT / "res")
    parser.add_argument("--existing-res", type=Path, default=REPOSITORY_ROOT / "res")
    parser.add_argument(
        "--raw-output",
        type=Path,
        default=REPOSITORY_ROOT / "target/nte-direct-export",
        help="CUE4Parse 原始 JSON 和报告目录",
    )
    parser.add_argument(
        "--dotnet",
        type=Path,
        default=REPOSITORY_ROOT / "tools/external/dotnet10/dotnet.exe",
    )
    parser.add_argument(
        "--probe",
        type=Path,
        default=(
            REPOSITORY_ROOT
            / "tools/cue4parse_probe/bin/Release/net10.0/Cue4ParseProbe.dll"
        ),
    )
    parser.add_argument("--prune-stale-characters", action="store_true")
    args = parser.parse_args()

    try:
        tables = selected_tables(args.table)
        raw_root = args.raw_output.resolve()
        output_res = args.output_res.resolve()
        raw_root.mkdir(parents=True, exist_ok=True)
        output_res.mkdir(parents=True, exist_ok=True)

        probe_report = run_probe(args, tables, raw_root)
        transformed = transform_tables(args, tables, raw_root, output_res)
        report = {
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "paks_directory": str(args.paks_dir.resolve()),
            "usmap": str(args.usmap.resolve()) if args.usmap is not None else None,
            "selected_tables": tables,
            "available_file_count": probe_report.get("available_file_count"),
            **transformed,
        }
        report_path = output_res / "data/direct_export_report.json"
        pipeline.write_json(report_path, report)
        print(f"res 数据已生成到: {output_res}")
        print(f"导出报告: {report_path}")
        print(
            "数据表行数: "
            + ", ".join(
                f"{name}={count}" for name, count in transformed["counts"].items()
            )
        )
        return 0
    except (ExportError, pipeline.PipelineError, OSError, json.JSONDecodeError) as error:
        print(f"错误: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
