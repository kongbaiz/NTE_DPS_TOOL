#!/usr/bin/env python3
"""Build the compact runtime scene index from exported UE world data."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


DEFAULT_WORLD = Path(
    "target/world-resource-probe/Maps/Map_bigworld/XL_map_bigworld_test.json"
)
DEFAULT_ACTIVITY_TABLE = Path("data/DataTable/ActivityLocalDataLayerDataTable.json")
DEFAULT_VISION_TABLE = Path("data/DataTable/Vision/DT_Vision.json")
DEFAULT_ADV_VISION_TABLE = Path("data/DataTable/Vision/DT_AdvVision.json")
DEFAULT_ADV_VISION_SCENE_TABLE = Path(
    "data/DataTable/Vision/DT_AdvVisionSceneData.json"
)
DEFAULT_MONSTER_MANUAL_TABLE = Path("data/DataTable/DT_MonsterManualConfig.json")
DEFAULT_OUTPUT = Path("res/data/scenes/scene_index.json")

DISPLAY_NAMES = {
    "W_Unload_Abyss_Entrance": "深渊入口",
    "W_Unload_Abyss_Battle_001": "深渊·战斗一区",
    "W_Unload_Abyss_Battle_002": "深渊·战斗二区",
    "W_Unload_RainLiyuu": "异象委托·天弃之子",
    "DL_Trinity_Parking": "场景切换中",
    "DL_ManorBoss": "塞润尼缇庄园",
    "W_Unload_CloneResource_01": "胡迪尼的魔术舞台",
    "W_Unload_CloneResource_02": "胡迪尼的魔术舞台",
    "W_Unload_CloneTalent01": "胡迪尼的诡计舞台",
    "W_Unload_CloneTalent02": "胡迪尼的诡计舞台",
    "W_Unload_CloneForkBreakthrough_01": "泡影罐头工厂",
    "W_Unload_CloneForkBreakthrough_02": "泡影罐头工厂",
    "W_Unload_CloneEquipment_01": "兔子洞",
    "W_Unload_CloneEquipment_02": "兔子洞",
}


def object_asset_name(value: Any) -> str:
    if not isinstance(value, dict):
        return ""
    object_name = value.get("ObjectName", "")
    if "'" in object_name:
        return object_name.split("'", 1)[1].rstrip("'")
    path = value.get("ObjectPath", "")
    return path.rsplit("/", 1)[-1].split(".", 1)[0]


def category_for(asset_name: str, activity_name: str | None) -> tuple[str, int] | None:
    lowered = asset_name.lower()
    if "abyss" in lowered:
        return "abyss", 100
    if "boss" in lowered or "battle" in lowered:
        return "battle", 90
    if "worldanomaly" in lowered or "vision" in lowered or activity_name:
        return "anomaly", 80
    if "museum" in lowered or "dungeon" in lowered or "clone" in lowered:
        return "instance", 70
    return None


def display_name_for(asset_name: str, activity_name: str | None) -> str:
    if asset_name in DISPLAY_NAMES:
        return DISPLAY_NAMES[asset_name]
    if activity_name:
        return activity_name
    name = asset_name
    for prefix in ("W_Unload_", "W_load_", "W_Art_load_", "DL_"):
        if name.startswith(prefix):
            name = name[len(prefix) :]
            break
    return name.replace("_", " ")


def table_rows(path: Path) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as file:
        document = json.load(file)
    for export in document:
        rows = export.get("Rows")
        if not isinstance(rows, dict):
            continue
        return rows
    return {}


def localized_text(value: Any) -> str | None:
    if not isinstance(value, dict):
        return None
    return value.get("LocalizedString") or value.get("SourceString")


def clean_display_name(value: str) -> str:
    return value.strip().strip("「」")


def vision_names(path: Path) -> dict[str, str]:
    return {
        row_name: clean_display_name(name)
        for row_name, row in table_rows(path).items()
        if (name := localized_text(row.get("VisionName")))
    }


def activity_assets(path: Path) -> dict[str, tuple[str, str | None]]:
    result: dict[str, tuple[str, str | None]] = {}
    for row_name, row in table_rows(path).items():
        condition = row.get("ActiveCondition") or {}
        vision_id = condition.get("VisionID")
        for asset in row.get("DataLayerAssets", []):
            asset_path = asset.get("AssetPathName", "")
            asset_name = asset_path.rsplit("/", 1)[-1].split(".", 1)[0]
            if asset_name:
                result.setdefault(asset_name, (row_name, vision_id))
    return result


def advanced_vision_assets(path: Path) -> dict[str, str]:
    result: dict[str, str] = {}
    for row_name, row in table_rows(path).items():
        vision_id = re.sub(r"_\d+$", "", row_name)
        for asset in row.get("TransformedLoadLayerAssets", []):
            asset_path = asset.get("AssetPathName", "")
            asset_name = asset_path.rsplit("/", 1)[-1].split(".", 1)[0]
            if asset_name:
                result.setdefault(asset_name, vision_id)
    return result


def world_boss_visions(path: Path) -> list[tuple[str, str]]:
    result = []
    for row_name, row in table_rows(path).items():
        vision_id = row.get("VisionID")
        world_boss_id = row.get("WorldBossID")
        if not vision_id or vision_id == "None" or not world_boss_id or world_boss_id == "None":
            continue
        match = re.search(r"boss_0*(\d+)", row_name, re.IGNORECASE)
        if match:
            result.append((match.group(1), vision_id))
    return result


def inferred_vision_id(
    asset_name: str,
    activity_name: str | None,
    names: dict[str, str],
    world_bosses: list[tuple[str, str]],
) -> str | None:
    combined = f"{asset_name} {activity_name or ''}".lower()
    if "schoolmyth" in combined:
        return "Vision_SchoolDoll"
    for boss_number, vision_id in world_bosses:
        number = int(boss_number)
        if re.search(rf"boss_0*{number}(?:\D|$)", combined):
            return vision_id
        codename = vision_id.removeprefix("Vision_").lower()
        if codename and codename in combined:
            return vision_id
    for vision_id in names:
        codename = vision_id.removeprefix("Vision_")
        codename = re.sub(r"_?0*\d+$", "", codename).lower()
        if len(codename) >= 5 and codename in combined:
            return vision_id
    return None


def data_layers(world: list[dict[str, Any]]) -> dict[str, str]:
    result: dict[str, str] = {}
    for export in world:
        if export.get("Type") != "DataLayerInstanceWithAsset":
            continue
        identifier = export.get("Name", "")
        asset_name = object_asset_name(
            export.get("Properties", {}).get("DataLayerAsset")
        )
        if identifier.startswith("DataLayer_") and asset_name:
            result[identifier] = asset_name
    return result


def cell_data_layers(export: dict[str, Any]) -> list[str]:
    value = export.get("Properties", {}).get("DataLayers", [])
    if isinstance(value, dict):
        value = value.get("DataLayers", [])
    return [item for item in value if isinstance(item, str)]


def build_index(
    world_path: Path,
    activity_path: Path,
    vision_path: Path,
    adv_vision_path: Path,
    adv_scene_path: Path,
    monster_manual_path: Path,
    include_all: bool = False,
) -> dict[str, Any]:
    with world_path.open("r", encoding="utf-8") as file:
        world = json.load(file)
    activities = activity_assets(activity_path)
    names = vision_names(vision_path)
    adv_names = vision_names(adv_vision_path)
    adv_assets = advanced_vision_assets(adv_scene_path)
    world_bosses = world_boss_visions(monster_manual_path)
    layers = data_layers(world)

    scenes: dict[str, dict[str, Any]] = {}
    layer_to_scene: dict[str, str] = {}
    for identifier, asset_name in layers.items():
        activity_name = None
        display_name = None
        forced_category = None
        forced_priority = None
        if asset_name == "DL_Trinity_Parking":
            display_name = DISPLAY_NAMES[asset_name]
            forced_category = "transition"
            forced_priority = 255
        elif adv_vision_id := adv_assets.get(asset_name):
            if name := adv_names.get(adv_vision_id):
                display_name = f"异象追猎·{name}"
                forced_category = "battle"
                forced_priority = 110
        elif activity := activities.get(asset_name):
            activity_name, vision_id = activity
            vision_id = vision_id or inferred_vision_id(
                asset_name, activity_name, names, world_bosses
            )
            if vision_id and (name := names.get(vision_id)):
                display_name = f"异象委托·{name}"
                if "boss" not in asset_name.lower() and "wonder" not in asset_name.lower():
                    forced_category = "anomaly"
                    forced_priority = 100
        elif vision_id := inferred_vision_id(
            asset_name, None, names, world_bosses
        ):
            if name := names.get(vision_id):
                display_name = f"异象委托·{name}"
                activity_name = vision_id
        classification = (
            (forced_category, forced_priority)
            if forced_category and forced_priority is not None
            else category_for(asset_name, activity_name)
        )
        if classification is None:
            if not include_all:
                continue
            classification = ("other", 10)
        category, priority = classification
        category = forced_category or category
        priority = forced_priority or priority
        scene_id = asset_name
        if "abyss" in asset_name.lower():
            scene_id = "Abyss"
            display_name = "深渊"
            category = "abyss"
            priority = 120
        elif display_name and display_name.startswith("异象委托·"):
            scene_id = display_name
        scene = scenes.setdefault(
            scene_id,
            {
                "id": scene_id,
                "display_name": display_name
                or display_name_for(asset_name, activity_name),
                "category": category,
                "priority": priority,
                "tokens": [],
            },
        )
        scene["tokens"].append(identifier)
        layer_to_scene[identifier] = scene_id

    # Cell identifiers are useful when packets only contain a World Partition path.
    # Keep them only for high-priority combat scenes to avoid a large runtime index.
    for export in world:
        if "RuntimeLevelStreamingCell" not in export.get("Type", ""):
            continue
        cell_name = export.get("Name", "")
        if not cell_name:
            continue
        for layer in cell_data_layers(export):
            scene_id = layer_to_scene.get(layer)
            if scene_id is None:
                continue
            scene = scenes[scene_id]
            if include_all and scene["priority"] >= 90:
                scene["tokens"].append(f"/_Generated_/{cell_name}")

    for scene in scenes.values():
        scene["tokens"] = sorted(set(scene["tokens"]))

    return {
        "version": 1,
        "source": world_path.as_posix(),
        "include_all": include_all,
        "scenes": sorted(
            scenes.values(),
            key=lambda scene: (-scene["priority"], scene["id"]),
        ),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--world", type=Path, default=DEFAULT_WORLD)
    parser.add_argument("--activity-table", type=Path, default=DEFAULT_ACTIVITY_TABLE)
    parser.add_argument("--vision-table", type=Path, default=DEFAULT_VISION_TABLE)
    parser.add_argument("--adv-vision-table", type=Path, default=DEFAULT_ADV_VISION_TABLE)
    parser.add_argument(
        "--adv-vision-scene-table",
        type=Path,
        default=DEFAULT_ADV_VISION_SCENE_TABLE,
    )
    parser.add_argument(
        "--monster-manual-table",
        type=Path,
        default=DEFAULT_MONSTER_MANUAL_TABLE,
    )
    parser.add_argument(
        "--include-all",
        action="store_true",
        help="include every DataLayer asset, using category 'other' when unclassified",
    )
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()

    index = build_index(
        args.world,
        args.activity_table,
        args.vision_table,
        args.adv_vision_table,
        args.adv_vision_scene_table,
        args.monster_manual_table,
        args.include_all,
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(index, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    token_count = sum(len(scene["tokens"]) for scene in index["scenes"])
    print(
        f"wrote {args.output}: {len(index['scenes'])} scenes, "
        f"{token_count} lookup tokens"
    )


if __name__ == "__main__":
    main()
