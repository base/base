#!/usr/bin/env python3
"""Render workspace-only crate dependencies; requires Cargo and Graphviz."""

from collections import defaultdict
import json
from pathlib import Path
import subprocess


def main():
    root = Path(__file__).resolve().parents[3]
    output = root / "docs/architecture"
    output.mkdir(parents=True, exist_ok=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
        cwd=root,
    ))
    packages = {
        p["name"]: p for p in metadata["packages"]
        if p["id"] in metadata["workspace_members"]
    }
    groups = defaultdict(list)
    for name, package in packages.items():
        relative = Path(package["manifest_path"]).relative_to(root)
        group = relative.parts[1] if relative.parts[0] == "crates" else relative.parts[0]
        groups[group].append(name)

    edges = {}
    for name, package in packages.items():
        for dependency in package["dependencies"]:
            if not dependency.get("path") or dependency["name"] not in packages:
                continue
            if name == dependency["name"]:
                continue
            key = (name, dependency["name"])
            unconditional = (
                dependency["kind"] is None
                and not dependency["optional"]
                and dependency["target"] is None
            )
            edges[key] = edges.get(key, False) or unconditional

    lines = [
        "digraph crates {",
        'graph [rankdir=LR, compound=true, bgcolor="white", fontname="sans-serif", '
        'label="Base internal crate dependencies\\nSolid: unconditional runtime dependency. '
        'Dashed: optional, target-specific, build or test dependency.", labelloc=t];',
        'node [shape=box, style="rounded,filled", fillcolor="#f4f7fb", '
        'fontname="sans-serif", fontsize=10];',
        'edge [color="#607080", arrowsize=0.55];',
    ]
    for index, (group, names) in enumerate(sorted(groups.items())):
        lines.extend([f"subgraph cluster_{index} {{", f'label={json.dumps(group)}; color="#ccd5df";'])
        lines.extend(json.dumps(name) + ";" for name in sorted(names))
        lines.append("}")
    for (source, target), solid in sorted(edges.items()):
        style = "solid" if solid else "dashed"
        lines.append(f"{json.dumps(source)} -> {json.dumps(target)} [style={style}];")
    lines.append("}")
    dot = output / "crates.dot"
    dot.write_text("\n".join(lines) + "\n")
    subprocess.run(["dot", "-Tsvg", str(dot), "-o", str(output / "crates.svg")], check=True)
    print(f"{len(packages)} crates, {len(edges)} internal edges")


if __name__ == "__main__":
    main()
