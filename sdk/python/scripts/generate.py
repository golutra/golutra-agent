from __future__ import annotations

import json
import re
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


def render_typed_dict(
    name: str, properties: dict[str, Any], required: set[str]
) -> list[str]:
    lines = [f"class {name}(TypedDict, total=False):"]
    if not properties:
        lines.append("    pass")
    for field, field_schema in properties.items():
        annotation = schema_type(field_schema)
        wrapper = "Required" if field in required else "NotRequired"
        lines.append(f"    {field}: {wrapper}[{annotation}]")
    return lines


def pascal_case(value: str) -> str:
    words = [word for word in re.split(r"[^A-Za-z0-9]+", value) if word]
    rendered = "".join(word[:1].upper() + word[1:] for word in words)
    return rendered if rendered and not rendered[0].isdigit() else f"Variant{rendered}"


def variant_suffix(schema: dict[str, Any], index: int) -> str:
    properties = schema.get("properties")
    if isinstance(properties, dict):
        for discriminator in ("type", "kind"):
            value = properties.get(discriminator, {}).get("const")
            if isinstance(value, str):
                return pascal_case(value)
        if len(properties) == 1:
            return pascal_case(next(iter(properties)))
    return f"Variant{index + 1}"


def render_definition(name: str, schema: dict[str, Any]) -> tuple[list[str], list[str]]:
    variants = schema.get("oneOf")
    if isinstance(variants, list) and variants:
        base_properties = schema.get("properties", {})
        if not isinstance(base_properties, dict):
            base_properties = {}
        base_required = set(schema.get("required", []))
        lines: list[str] = []
        rendered_variants: list[str] = []
        auxiliary_names: list[str] = []
        used_names: set[str] = set()
        for index, variant in enumerate(variants):
            properties = variant.get("properties") if isinstance(variant, dict) else None
            if isinstance(properties, dict):
                variant_name = f"{name}{variant_suffix(variant, index)}"
                if variant_name in used_names:
                    variant_name = f"{variant_name}{index + 1}"
                used_names.add(variant_name)
                merged_properties = {**base_properties, **properties}
                required = base_required | set(variant.get("required", []))
                lines.extend(render_typed_dict(variant_name, merged_properties, required))
                lines.append("")
                rendered_variants.append(variant_name)
                auxiliary_names.append(variant_name)
            else:
                rendered_variants.append(schema_type(variant))
        rendered_variants = list(dict.fromkeys(rendered_variants))
        lines.append(f"{name}: TypeAlias = " + " | ".join(rendered_variants))
        return lines, auxiliary_names

    properties = schema.get("properties")
    if schema.get("type") == "object" and isinstance(properties, dict):
        return render_typed_dict(name, properties, set(schema.get("required", []))), []
    return [f"{name}: TypeAlias = {schema_type(schema)}"], []


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
    auxiliary_names: list[str] = []
    for name in sorted(definitions):
        rendered, auxiliary = render_definition(name, definitions[name])
        lines.extend(rendered)
        auxiliary_names.extend(auxiliary)
        lines.append("")
    names = sorted([*definitions, *auxiliary_names])
    lines.append("__all__ = [")
    lines.extend(f'    "{name}",' for name in names)
    lines.append("]")
    lines.append("")
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT_PATH.write_text("\n".join(lines), encoding="utf-8")


if __name__ == "__main__":
    main()
