#!/usr/bin/env python3
"""Package compiler output, not deployment scripts or precomputed chain state."""

import json
import pathlib
import sys


def export(source, destination, revision):
    output = pathlib.Path(destination)
    output.mkdir(parents=True, exist_ok=True)
    constants = {}
    for path in sorted((pathlib.Path(source) / "forge-artifacts").rglob("*.json")):
        artifact = json.loads(path.read_text())
        metadata = artifact.get("metadata", {})
        targets = metadata.get("settings", {}).get("compilationTarget", {})
        if len(targets) != 1:
            continue
        file, name = next(iter(targets.items()))
        if name in ("Preinstalls", "Predeploys"):
            declarations = next(n for n in artifact["ast"]["nodes"]
                                if n.get("nodeType") == "ContractDefinition")["nodes"]
            values = {}
            for node in declarations:
                if node.get("nodeType") != "VariableDeclaration" or not node.get("constant"):
                    continue
                value = node.get("value", {})
                kind = node["typeDescriptions"]["typeString"]
                if kind not in ("address", "bytes"):
                    continue
                if value.get("nodeType") != "Literal":
                    raise ValueError(f"nonliteral {name}.{node['name']}; update artifact export")
                values[node["name"]] = (value["value"] if kind == "address"
                                        else "0x" + value["hexValue"])
            if name in constants and values != constants[name]:
                raise ValueError(f"inconsistent compiler variants for {name}")
            constants[name] = values
        # Keep the selected, unambiguous compiler profile, not .dispute/other variants.
        if path.name != name + ".json" or not (
                file.startswith("src/") or name == "AddressManagerDeployer"):
            continue
        if not artifact.get("bytecode", {}).get("object", "0x").removeprefix("0x"):
            continue
        target = output / (name + ".json")
        if target.exists():
            raise ValueError(f"ambiguous artifact {name}")
        target.write_text(json.dumps({key: artifact[key] for key in
                                      ("abi", "bytecode", "deployedBytecode")}) + "\n")
    preinstalls = constants["Preinstalls"]
    alloc = {}
    for name, address in preinstalls.items():
        if not address.startswith("0x") or len(address) != 42:
            continue
        code = preinstalls.get(name + "Code", preinstalls.get(name + "TemplateCode"))
        if code is not None:
            alloc[address] = {"nonce": "0x1", "balance": "0x0", "code": code}
        elif name.endswith("Sender"):
            alloc[address] = {"nonce": "0x1", "balance": "0x0"}
        else:
            raise ValueError(f"missing preinstall bytecode: {name}")
    (output / "preinstalls.json").write_text(json.dumps(alloc, sort_keys=True) + "\n")
    (output / "predeploys.json").write_text(json.dumps(constants["Predeploys"], sort_keys=True) + "\n")
    (output / "revision.txt").write_text(revision.strip() + "\n")


if __name__ == "__main__":
    export(*sys.argv[1:])
