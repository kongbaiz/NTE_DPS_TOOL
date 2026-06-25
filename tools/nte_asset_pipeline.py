#!/usr/bin/env python3
"""Build the DPS tool resources from an exported NTE asset tree.

The exporter stage is intentionally separate. FModel, CUE4Parse, or another
authorized Unreal exporter may be used as long as it produces JSON and PNG
files with paths equivalent to the NTE_Assets repository layout.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REQUIRED_TABLES = {
    "characters": "DataTable/Character/DT_Character.json",
    "gameplay_effect_mapping": "DataTable/Skill/DT_GameplayEffectMappingData.json",
    "skill_damage": "DataTable/Skill/DT_SkillDamageData.json",
    "wooden_damage_descriptions": "DataTable/Skill/Wooden/DT_WoodenStructData.json",
    "ability_tips": "DataTable/Skill/DT_GameplayAbilityTipsData.json",
    "reaction_data": "DataTable/Reaction/DT_ReactionData.json",
    "reaction_detail": "DataTable/Reaction/DT_ReactionDetailUIData.json",
    "reaction_damage": "DataTable/Reaction/DT_ReactionDamageData.json",
    "reaction_elements": "DataTable/Reaction/DT_ReactionElementTypeData.json",
    "reaction_extensions": "DataTable/Reaction/DT_ReactionExtendDataTable.json",
}

PROGRAM_TABLE_OUTPUTS = {
    "gameplay_effect_mapping": "data/skills/gameplay_effect_mapping.json",
    "skill_damage": "data/skills/skill_damage.json",
    "wooden_damage_descriptions": "data/skills/wooden_damage_descriptions.json",
}

ZH_CN_LOCALIZATION_PATH = "Localization/zh-CN/game.json"
ABYSS_SEASON_NAMES_OUTPUT = "data/abyss/season_names_zh_cn.json"
ABYSS_SEASON_NAME_RE = re.compile(r"^Abyss_(\d+)_name$")

ATTRIBUTE_NAMES = {
    "CHARACTER_ELEMENT_TYPE_COSMOS": "光",
    "CHARACTER_ELEMENT_TYPE_NATURE": "灵",
    "CHARACTER_ELEMENT_TYPE_INCANTATION": "咒",
    "CHARACTER_ELEMENT_TYPE_CHAOS": "暗",
    "CHARACTER_ELEMENT_TYPE_PSYCHE": "魂",
    "CHARACTER_ELEMENT_TYPE_LAKSHANA": "相",
}

ATTRIBUTE_IMAGES = {
    "灵": "UI_avatarbg_Icon_01.png",
    "相": "UI_avatarbg_Icon_02.png",
    "暗": "UI_avatarbg_Icon_03.png",
    "光": "UI_avatarbg_Icon_04.png",
    "魂": "UI_avatarbg_Icon_05.png",
    "咒": "UI_avatarbg_Icon_06.png",
}

ASSET_PATH_RE = re.compile(r"^(?P<package>/Game/.+?)\.(?P<object>[^./]+)(?:_C)?$")
PLAYER_NAME_RE = re.compile(r"player_\d+_(.+)$", re.IGNORECASE)


class PipelineError(RuntimeError):
    pass


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise PipelineError(f"缺少文件: {path}") from error
    except json.JSONDecodeError as error:
        raise PipelineError(f"JSON 解析失败: {path}: {error}") from error


def write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    temporary.replace(path)


def table_rows(path: Path) -> dict[str, Any]:
    data = read_json(path)
    if isinstance(data, list) and data:
        data = data[0]
    if not isinstance(data, dict):
        raise PipelineError(f"无法识别 DataTable 格式: {path}")
    rows = data.get("Rows", data)
    if not isinstance(rows, dict):
        raise PipelineError(f"DataTable 缺少 Rows 对象: {path}")
    return rows


def enum_tail(value: Any) -> str:
    return str(value or "").rsplit("::", 1)[-1]


def text_value(value: Any, key: str) -> str:
    if not isinstance(value, dict):
        return ""
    result = value.get(key)
    return result if isinstance(result, str) else ""


def asset_basename(value: Any) -> str:
    if isinstance(value, dict):
        value = value.get("AssetPathName", "")
    if not isinstance(value, str) or not value:
        return ""
    match = ASSET_PATH_RE.match(value)
    if match:
        return match.group("object").removesuffix("_C")
    return value.rsplit("/", 1)[-1].split(".", 1)[0].removesuffix("_C")


def exported_asset_candidates(assets_root: Path, asset_path: str, suffix: str) -> list[Path]:
    match = ASSET_PATH_RE.match(asset_path)
    package = match.group("package") if match else asset_path.split(".", 1)[0]
    relative = package.removeprefix("/Game/").lstrip("/")
    candidates = [assets_root / f"{relative}{suffix}"]
    if relative.startswith("UI/"):
        candidates.append(assets_root / f"{relative.removeprefix('UI/')}{suffix}")
    return candidates


def find_exported_asset(assets_root: Path, asset_path: str, suffix: str) -> Path | None:
    for candidate in exported_asset_candidates(assets_root, asset_path, suffix):
        if candidate.is_file():
            return candidate
    return None


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_revision(path: Path) -> str | None:
    try:
        result = subprocess.run(
            [
                "git",
                "-c",
                f"safe.directory={path.resolve().as_posix()}",
                "-C",
                str(path),
                "rev-parse",
                "HEAD",
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return result.stdout.strip() or None


def load_existing_characters(existing_res: Path | None) -> dict[str, Any]:
    if existing_res is None:
        return {}
    path = existing_res / "data/characters/characters.json"
    if not path.is_file():
        return {}
    data = read_json(path)
    characters = data.get("characters", {}) if isinstance(data, dict) else {}
    return characters if isinstance(characters, dict) else {}


def character_codename(row: dict[str, Any], fallback: str) -> str:
    element = row.get("ElementData", {})
    actor = element.get("CharacterActorClass", {}) if isinstance(element, dict) else {}
    stem = asset_basename(actor)
    match = PLAYER_NAME_RE.match(stem)
    return match.group(1) if match else fallback


def build_characters(
    assets_root: Path,
    output_res: Path,
    rows: dict[str, Any],
    existing: dict[str, Any],
    prune_stale: bool,
) -> tuple[dict[str, Any], dict[str, Any]]:
    generated: dict[str, Any] = {}
    missing_avatars: list[str] = []
    copied_avatars = 0

    for character_id, row in sorted(rows.items()):
        if not isinstance(row, dict):
            continue
        name = row.get("ItemName", {})
        name_zh = text_value(name, "SourceString")
        name_en = text_value(name, "LocalizedString")
        element = row.get("ElementData", {})
        element_enum = enum_tail(
            element.get("CharacterElementType") if isinstance(element, dict) else ""
        )
        attribute = ATTRIBUTE_NAMES.get(element_enum, element_enum)
        old = existing.get(character_id, {})
        if not isinstance(old, dict):
            old = {}

        icon = row.get("ItemIconBig", {})
        icon_path = icon.get("AssetPathName", "") if isinstance(icon, dict) else ""
        source_icon = find_exported_asset(assets_root, icon_path, ".png")
        avatar_name = asset_basename(icon_path)
        avatar_relative = f"res/images/characters/{avatar_name}.png" if avatar_name else ""
        if source_icon and avatar_name:
            destination = output_res / "images/characters" / f"{avatar_name}.png"
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source_icon, destination)
            copied_avatars += 1
        else:
            missing_avatars.append(character_id)
            avatar_relative = str(old.get("avatar", avatar_relative))

        generated[character_id] = {
            "attribute": attribute or str(old.get("attribute", "")),
            "avatar": avatar_relative,
            "codename": str(
                old.get("codename")
                or character_codename(row, name_en or name_zh or character_id)
            ),
            "name_en": name_en or str(old.get("name_en", "")),
            "name_zh": name_zh or str(old.get("name_zh", "")),
            "verified": bool(old.get("verified", True)),
        }

    stale_ids = sorted(set(existing) - set(generated))
    if not prune_stale:
        for character_id in stale_ids:
            stale = existing[character_id]
            if isinstance(stale, dict):
                stale = dict(stale)
                stale.pop("color", None)
            generated[character_id] = stale

    write_json(
        output_res / "data/characters/characters.json",
        {"characters": dict(sorted(generated.items()))},
    )
    return generated, {
        "table_rows": len(rows),
        "output_rows": len(generated),
        "copied_avatars": copied_avatars,
        "missing_avatar_ids": missing_avatars,
        "preserved_stale_ids": [] if prune_stale else stale_ids,
        "pruned_stale_ids": stale_ids if prune_stale else [],
    }


def copy_attribute_images(assets_root: Path, output_res: Path) -> list[str]:
    missing: list[str] = []
    for attribute, filename in ATTRIBUTE_IMAGES.items():
        source = assets_root / "UI/Player" / filename
        if not source.is_file():
            missing.append(attribute)
            continue
        destination = output_res / "images/attributes" / filename
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    return missing


def build_ability_index(rows: dict[str, Any]) -> dict[str, Any]:
    abilities: dict[str, Any] = {}
    for ability_id, row in sorted(rows.items()):
        if not isinstance(row, dict):
            continue
        descriptions = []
        for description in row.get("AbilityDescription", []):
            if not isinstance(description, dict):
                continue
            descriptions.append(
                {
                    "type": enum_tail(description.get("AbilityDesType")),
                    "title_zh": text_value(description.get("DescriptionTitle"), "SourceString"),
                    "title_en": text_value(
                        description.get("DescriptionTitle"), "LocalizedString"
                    ),
                    "description_zh": text_value(
                        description.get("Description"), "SourceString"
                    ),
                    "description_en": text_value(
                        description.get("Description"), "LocalizedString"
                    ),
                    "short_description_zh": text_value(
                        description.get("ShortDescription"), "SourceString"
                    ),
                    "short_description_en": text_value(
                        description.get("ShortDescription"), "LocalizedString"
                    ),
                }
            )
        abilities[ability_id] = {
            "name_zh": text_value(row.get("Name"), "SourceString"),
            "name_en": text_value(row.get("Name"), "LocalizedString"),
            "descriptions": descriptions,
        }
    return {"abilities": abilities}


def build_reaction_index(tables: dict[str, dict[str, Any]]) -> dict[str, Any]:
    reaction_data = tables["reaction_data"]
    details = tables["reaction_detail"]
    damage_rows = tables["reaction_damage"]
    element_rows = tables["reaction_elements"]

    elements = {}
    for key, row in sorted(element_rows.items()):
        text = row.get("ElementText", {}) if isinstance(row, dict) else {}
        elements[key] = {
            "name_zh": text_value(text, "SourceString"),
            "name_en": text_value(text, "LocalizedString"),
        }

    damage_by_type: dict[str, list[dict[str, Any]]] = {}
    for effect, row in sorted(damage_rows.items()):
        if not isinstance(row, dict):
            continue
        reaction_type = enum_tail(row.get("ProduceReactionType"))
        damage_by_type.setdefault(reaction_type, []).append(
            {"gameplay_effect": effect, "base_damage": row.get("ReactionDamageArray", [])}
        )

    reactions = {}
    for detail_id, detail in sorted(details.items(), key=lambda item: str(item[0])):
        if not isinstance(detail, dict):
            continue
        reaction_type = f"REACTION_RESULT_TYPE_{detail_id}"
        base = reaction_data.get(reaction_type, {})
        if not isinstance(base, dict):
            base = {}
        reactions[str(detail_id)] = {
            "reaction_type": reaction_type,
            "name_zh": text_value(detail.get("ReactionResultName"), "SourceString"),
            "name_en": text_value(detail.get("ReactionResultName"), "LocalizedString"),
            "description_zh": text_value(
                detail.get("ReactionResultDesc"), "SourceString"
            ),
            "description_en": text_value(
                detail.get("ReactionResultDesc"), "LocalizedString"
            ),
            "elements": detail.get("ReactionElementArray", []),
            "default_damage_effect": base.get("DefaultDamageGE", "None"),
            "damage_effects": damage_by_type.get(reaction_type, []),
            "character_abilities": detail.get("InnerData", []),
        }

    return {
        "elements": elements,
        "reactions": reactions,
        "extensions": tables["reaction_extensions"],
    }


def build_abyss_season_names(localization: dict[str, Any]) -> dict[str, str]:
    season_names: dict[str, str] = {}
    collect_abyss_season_names(localization, season_names)
    return dict(sorted(season_names.items(), key=lambda item: int(item[0])))


def collect_abyss_season_names(value: Any, season_names: dict[str, str]) -> None:
    if not isinstance(value, dict):
        return
    for key, child in value.items():
        match = ABYSS_SEASON_NAME_RE.fullmatch(key)
        if match and isinstance(child, str):
            name = child.strip()
            if name and "," not in name:
                season_names[str(int(match.group(1)))] = name
        collect_abyss_season_names(child, season_names)


def list_difference(left: set[str], right: set[str]) -> list[str]:
    return sorted(left - right)


def build_coverage(
    tables: dict[str, dict[str, Any]],
    characters_report: dict[str, Any],
    missing_attribute_images: list[str],
) -> dict[str, Any]:
    mapping_rows = tables["gameplay_effect_mapping"]
    skill_rows = tables["skill_damage"]
    wooden_rows = tables["wooden_damage_descriptions"]
    ability_rows = tables["ability_tips"]

    mapping_effects = {
        asset_basename(row.get("GameplayEffectClass"))
        for row in mapping_rows.values()
        if isinstance(row, dict)
    }
    mapping_effects.discard("")
    skill_effects = set(skill_rows)
    wooden_effects = set(wooden_rows)
    skill_abilities = {
        str(row.get("GAName"))
        for row in skill_rows.values()
        if isinstance(row, dict) and row.get("GAName") not in (None, "", "None")
    }

    return {
        "counts": {
            "gameplay_effect_mapping": len(mapping_rows),
            "mapped_effect_names": len(mapping_effects),
            "skill_damage": len(skill_rows),
            "wooden_descriptions": len(wooden_rows),
            "ability_tips": len(ability_rows),
            "reaction_details": len(tables["reaction_detail"]),
            "characters": characters_report["output_rows"],
        },
        "coverage": {
            "skill_damage_in_mapping": len(skill_effects & mapping_effects),
            "skill_damage_with_wooden_description": len(skill_effects & wooden_effects),
            "skill_abilities_with_tip": len(skill_abilities & set(ability_rows)),
        },
        "missing": {
            "skill_damage_not_in_mapping": list_difference(skill_effects, mapping_effects),
            "mapped_effect_without_skill_damage": list_difference(
                mapping_effects, skill_effects
            ),
            "skill_damage_without_wooden_description": list_difference(
                skill_effects, wooden_effects
            ),
            "skill_ability_without_tip": list_difference(
                skill_abilities, set(ability_rows)
            ),
            "character_avatar_ids": characters_report["missing_avatar_ids"],
            "attribute_images": missing_attribute_images,
        },
        "characters": characters_report,
    }


def validate_assets_root(assets_root: Path) -> dict[str, Path]:
    paths = {name: assets_root / relative for name, relative in REQUIRED_TABLES.items()}
    missing = [str(path) for path in paths.values() if not path.is_file()]
    if missing:
        formatted = "\n  ".join(missing)
        raise PipelineError(f"导出目录缺少必要文件:\n  {formatted}")
    return paths


def build_resources(args: argparse.Namespace) -> int:
    assets_root = args.assets_root.resolve()
    output_res = args.output_res.resolve()
    existing_res = args.existing_res.resolve() if args.existing_res else None
    paths = validate_assets_root(assets_root)
    tables = {name: table_rows(path) for name, path in paths.items()}
    localization_path = assets_root / ZH_CN_LOCALIZATION_PATH

    for table_name, relative_output in PROGRAM_TABLE_OUTPUTS.items():
        destination = output_res / relative_output
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(paths[table_name], destination)

    existing_characters = load_existing_characters(existing_res)
    _, characters_report = build_characters(
        assets_root,
        output_res,
        tables["characters"],
        existing_characters,
        args.prune_stale_characters,
    )
    missing_attribute_images = copy_attribute_images(assets_root, output_res)

    ability_index = build_ability_index(tables["ability_tips"])
    reaction_index = build_reaction_index(tables)
    write_json(output_res / "data/skills/ability_tips.json", ability_index)
    write_json(output_res / "data/reactions/reactions.json", reaction_index)
    abyss_season_names: dict[str, str] = {}
    if localization_path.is_file():
        localization = read_json(localization_path)
        if isinstance(localization, dict):
            abyss_season_names = build_abyss_season_names(localization)
            write_json(output_res / ABYSS_SEASON_NAMES_OUTPUT, abyss_season_names)

    coverage = build_coverage(tables, characters_report, missing_attribute_images)
    write_json(output_res / "data/asset_report.json", coverage)

    manifest = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "assets_root": str(assets_root),
        "assets_git_revision": git_revision(assets_root),
        "inputs": {
            name: {
                "path": str(path.relative_to(assets_root)).replace("\\", "/"),
                "sha256": file_sha256(path),
            }
            for name, path in paths.items()
        }
        | (
            {
                "zh_cn_localization": {
                    "path": ZH_CN_LOCALIZATION_PATH,
                    "sha256": file_sha256(localization_path),
                }
            }
            if localization_path.is_file()
            else {}
        ),
        "outputs": {
            "program_tables": PROGRAM_TABLE_OUTPUTS,
            "characters": "data/characters/characters.json",
            "ability_tips": "data/skills/ability_tips.json",
            "reactions": "data/reactions/reactions.json",
            "abyss_season_names": ABYSS_SEASON_NAMES_OUTPUT,
            "coverage_report": "data/asset_report.json",
        },
    }
    write_json(output_res / "data/asset_manifest.json", manifest)

    counts = coverage["counts"]
    print("资源已生成")
    print(
        "关键数据: "
        f"映射 {counts['gameplay_effect_mapping']}，"
        f"伤害 {counts['skill_damage']}，"
        f"木桩描述 {counts['wooden_descriptions']}，"
        f"技能说明 {counts['ability_tips']}，"
        f"角色 {counts['characters']}，"
        f"环合 {counts['reaction_details']}"
    )
    print("覆盖率报告: data/asset_report.json")
    return 0


def inventory_game(args: argparse.Namespace) -> int:
    paks_dir = args.paks_dir.resolve()
    if not paks_dir.is_dir():
        raise PipelineError("Paks 目录不存在")
    files = []
    for path in sorted(paks_dir.iterdir()):
        if path.is_file() and path.suffix.lower() in {".pak", ".utoc", ".ucas", ".sig"}:
            files.append(
                {
                    "name": path.name,
                    "type": path.suffix.lower().lstrip("."),
                    "size": path.stat().st_size,
                    "modified_at": datetime.fromtimestamp(
                        path.stat().st_mtime, timezone.utc
                    ).isoformat(),
                }
            )
    result = {
        "paks_dir_name": paks_dir.name,
        "paths_redacted": True,
        "container_types": sorted({item["type"] for item in files}),
        "total_size": sum(item["size"] for item in files),
        "files": files,
        "required_exports": REQUIRED_TABLES,
        "required_image_roots": [
            "/Game/UI/UI_Icon/AvatarImage/256",
            "/Game/UI/Player/UI_avatarbg_Icon_01 through 06",
        ],
    }
    if args.output:
        write_json(args.output.resolve(), result)
        print(f"容器清单已写入: {args.output.name}")
    else:
        print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="NTE DPS Tool 专用资源解包后处理与校验脚本"
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    inventory = subparsers.add_parser("inventory", help="盘点客户端 Unreal 容器")
    inventory.add_argument("--paks-dir", type=Path, required=True)
    inventory.add_argument("--output", type=Path)
    inventory.set_defaults(handler=inventory_game)

    build = subparsers.add_parser("build", help="从已导出的资源目录生成项目资源")
    build.add_argument("--assets-root", type=Path, required=True)
    build.add_argument("--output-res", type=Path, default=Path("res"))
    build.add_argument(
        "--existing-res",
        type=Path,
        default=Path("res"),
        help="用于保留角色颜色和人工记录的现有 res 目录",
    )
    build.add_argument(
        "--prune-stale-characters",
        action="store_true",
        help="删除角色表中不存在的旧记录；默认保留",
    )
    build.set_defaults(handler=build_resources)
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    try:
        return args.handler(args)
    except PipelineError as error:
        print(f"错误: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
