#!/usr/bin/env python3
"""Build a compact runtime monster-class to Chinese-name index."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


DEFAULT_DATA_ROOT = Path("data/DataTable")
DEFAULT_OUTPUT = Path("res/data/monsters/monster_index.json")


def table_rows(path: Path) -> dict[str, Any]:
    document = json.loads(path.read_text(encoding="utf-8"))
    for export in document:
        rows = export.get("Rows")
        if isinstance(rows, dict):
            return rows
    return {}


def localized_text(value: Any) -> str | None:
    if not isinstance(value, dict):
        return None
    return value.get("LocalizedString") or value.get("SourceString")


def canonical_family(value: str) -> str | None:
    match = re.match(r"^(boss|mon)_0*(\d+)", value, re.IGNORECASE)
    if not match:
        return None
    return f"{match.group(1).lower()}_{int(match.group(2))}"


def useful_comment(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    value = value.strip()
    if not value or value.isdigit() or value.lower() in {"test", "none"}:
        return None
    return value


def normalize_alias(value: str) -> str:
    value = value.rsplit("/", 1)[-1].split(".", 1)[0]
    value = re.sub(r"_C$", "", value, flags=re.IGNORECASE)
    return value.lower()


def build_index(data_root: Path) -> dict[str, Any]:
    manual_rows = table_rows(data_root / "DT_MonsterManualConfig.json")
    family_names = {}
    for row_name, row in manual_rows.items():
        name = localized_text(row.get("MonsterName"))
        family = canonical_family(row_name)
        if family and name:
            family_names[family] = name

    aliases: dict[str, dict[str, str]] = {}
    family_aliases = {
        family: {"id": row_name, "display_name": name}
        for row_name, row in manual_rows.items()
        if (family := canonical_family(row_name))
        and (name := localized_text(row.get("MonsterName")))
    }
    sources = sorted((data_root / "Monster").glob("DT_MonsterStaticData*.json"))
    for source in sources:
        for row_name, row in table_rows(source).items():
            name = (
                localized_text(row.get("TextName"))
                or useful_comment(row.get("Comment"))
                or family_names.get(canonical_family(row_name) or "")
            )
            if not name:
                continue
            candidate_aliases = {row_name, *(row.get("Tags") or [])}
            family = canonical_family(row_name)
            if family and (
                family not in family_aliases or source.name.endswith("_Abyss.json")
            ):
                family_aliases[family] = {"id": row_name, "display_name": name}
            for alias in candidate_aliases:
                if not isinstance(alias, str) or not alias:
                    continue
                aliases.setdefault(
                    normalize_alias(alias),
                    {"id": row_name, "display_name": name},
                )
    for family, entry in family_aliases.items():
        aliases.setdefault(family, entry)

    return {
        "version": 1,
        "sources": [path.as_posix() for path in sources],
        "aliases": dict(sorted(aliases.items())),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-root", type=Path, default=DEFAULT_DATA_ROOT)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()
    index = build_index(args.data_root)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(index, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(f"wrote {args.output}: {len(index['aliases'])} aliases")


if __name__ == "__main__":
    main()
