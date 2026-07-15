from __future__ import annotations

import json
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[3]
SCHEMA_PATH = ROOT / "schemas" / "sdk-protocol.schema.json"
OUTPUT_PATH = ROOT / "sdk" / "python" / "src" / "golutra_sdk" / "generated.py"


def schema_type(schema: dict[str, Any] | bool) -> str:
    if schema is True:
        return "Any"
    if schema is False:
        return "Never"
    reference = schema.get("$ref")
    if isinstance(reference, str):
        return reference.rsplit("/", 1)[-1]
    if "const" in schema:
        return f"Literal[{schema['const']!r}]"
    enum = schema.get("enum")
    if isinstance(enum, list) and enum:
        return "Literal[" + ", ".join(repr(value) for value in enum) + "]"
    variants = schema.get("anyOf") or schema.get("oneOf")
    if isinstance(variants, list) and variants:
        rendered = list(dict.fromkeys(schema_type(variant) for variant in variants))
        return " | ".join(rendered)
    all_of = schema.get("allOf")
    if isinstance(all_of, list) and len(all_of) == 1:
        return schema_type(all_of[0])
    kind = schema.get("type")
    if isinstance(kind, list):
        rendered = list(dict.fromkeys(schema_type({**schema, "type": item}) for item in kind))
        return " | ".join(rendered)
    if kind == "string":
        return "str"
    if kind == "integer":
        return "int"
    if kind == "number":
        return "float"
    if kind == "boolean":
        return "bool"
    if kind == "null":
        return "None"
    if kind == "array":
        return f"list[{schema_type(schema.get('items', {}))}]"
    if kind == "object" or "properties" in schema:
        additional = schema.get("additionalProperties")
        if isinstance(additional, dict):
            return f"dict[str, {schema_type(additional)}]"
        return "dict[str, Any]"
    return "Any"


def render_definition(name: str, schema: dict[str, Any]) -> list[str]:
    properties = schema.get("properties")
    if schema.get("type") == "object" and isinstance(properties, dict):
        required = set(schema.get("required", []))
        lines = [f"class {name}(TypedDict, total=False):"]
        if not properties:
            lines.append("    pass")
        for field, field_schema in properties.items():
            annotation = schema_type(field_schema)
            wrapper = "Required" if field in required else "NotRequired"
            lines.append(f"    {field}: {wrapper}[{annotation}]")
        return lines
    return [f"{name}: TypeAlias = {schema_type(schema)}"]


def main() -> None:
    schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
    definitions = schema.get("$defs", {})
    lines = [
        '"""Generated from Golutra Rust protocol schemas. Do not edit manually."""',
        "",
        "from __future__ import annotations",
        "",
        "from typing import Any, Literal, Never, NotRequired, Required, TypeAlias, TypedDict",
        "",
    ]
    for name in sorted(definitions):
        lines.extend(render_definition(name, definitions[name]))
        lines.append("")
    names = sorted(definitions)
    lines.append("__all__ = [")
    lines.extend(f'    "{name}",' for name in names)
    lines.append("]")
    lines.append("")
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT_PATH.write_text("\n".join(lines), encoding="utf-8")


if __name__ == "__main__":
    main()
