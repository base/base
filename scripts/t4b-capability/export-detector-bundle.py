#!/usr/bin/env python3
"""Create the immutable, offline T4b capability detector bundle."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile

SCHEMA = "t4b-capability-detector-bundle/v1"
CONFIG = {
    "schema_version": "t4b-capability-scan-config/v1",
    "paths": {
        "submit_authority": "crates/execution/mev-trader-submit/src/tx_authority.rs",
        "submit_lib": "crates/execution/mev-trader-submit/src/lib.rs",
        "submit_cargo": "crates/execution/mev-trader-submit/Cargo.toml",
        "cli_source": "crates/execution/cli/src/mev_trader.rs",
        "cli_lib": "crates/execution/cli/src/lib.rs",
        "cli_cargo": "crates/execution/cli/Cargo.toml",
    },
    "checked_bindings_view": [
        "access_digest", "beryl_env", "deployment_witness", "executor", "frame",
        "frame_digest", "freshness_witness", "header_coinbase", "header_identity_digest",
        "kickback_recipient", "nonce_witness", "order_digest", "overlay_digest",
        "parent_hash", "parent_header", "plan_digest", "resolved_adapters", "route_digest",
        "route_hops", "route_pools", "route_protocols", "route_tokens", "sender",
        "shape_digest", "state_digest", "unsigned_signing_hash",
    ],
    "nested_getters": {
        "CheckedBerylEnvInputs": ["base_fee_per_gas", "block_number", "chain_id", "excess_blob_gas", "gas_limit", "prev_randao", "timestamp"],
        "DeploymentWitness": ["executor", "route_adapters", "validated_parent"],
        "NonceWitness": ["committed_nonce", "parent_hash", "pending_overlay_nonce", "sender", "shape_nonce"],
        "FreshnessWitness": ["parent_hash", "snapshot_identity_digest", "snapshot_parent_hash", "valid_until_block"],
    },
    "cli_root_exports": [
        "AuditPhase", "AuditedAccessKindV1", "AuditedAccessV1", "AuditedDatabase",
        "AuditedDatabaseError", "CandidateAccessAllowlistV1", "CandidateAccessedStateV1",
        "CandidateExecutionCardinalityV1", "CandidateStateCollectionError",
        "T4bCaptureDispositionV1", "T4bOverlayError", "T4bParentOverlayAdapter",
    ],
}

SCAN = r'''#!/usr/bin/env python3
"""Deterministic offline source/Cargo/Rust-structure scanner."""
from __future__ import annotations
import argparse, hashlib, json, re
from pathlib import Path

SCHEMA = "t4b-capability-scan/v1"
TEN = ["optional-reqwest-dependency", "mutant-feature", "feature-dependency-edge",
       "blocking-reqwest-import", "public-egress-probe", "egress-send-method",
       "loopback-url", "exact-egress-body", "observer-first-statement-call",
       "root-probe-reexport"]

def fail(message):
    raise SystemExit(message)

def normalized(root, value):
    root = root.resolve()
    candidate = (root / value).resolve() if not value.is_absolute() else value.resolve()
    try: return candidate.relative_to(root).as_posix(), candidate
    except ValueError: fail(f"path escapes root: {value}")

def strip_rust(source):
    out=[]; i=0; state="code"; depth=0
    while i < len(source):
        pair=source[i:i+2]; ch=source[i]
        if state == "code":
            if pair == "//": state="line"; out.extend("  "); i+=2; continue
            if pair == "/*": state="block"; depth=1; out.extend("  "); i+=2; continue
            if ch == '"': state="string"; out.append(' '); i+=1; continue
            if ch == "'" and i+2 < len(source) and source[i+2] == "'":
                out.extend("   "); i+=3; continue
            out.append(ch); i+=1; continue
        if state == "line":
            out.append("\n" if ch == "\n" else " "); i+=1
            if ch == "\n": state="code"
            continue
        if state == "block":
            if pair == "/*": depth+=1; out.extend("  "); i+=2
            elif pair == "*/": depth-=1; out.extend("  "); i+=2; state="code" if depth == 0 else state
            else: out.append("\n" if ch == "\n" else " "); i+=1
            continue
        if state == "string":
            if ch == "\\": out.extend("  "); i+=2
            elif ch == '"': out.append(' '); i+=1; state="code"
            else: out.append("\n" if ch == "\n" else " "); i+=1
    return "".join(out)

def block(source, marker):
    start=source.find(marker)
    if start < 0: fail(f"missing Rust item: {marker}")
    brace=source.find("{", start)
    if brace < 0: fail(f"missing item body: {marker}")
    depth=0
    for index in range(brace, len(source)):
        if source[index] == "{": depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0: return source[start:index+1]
    fail(f"unclosed Rust item: {marker}")

def methods(source, type_name):
    clean=strip_rust(source)
    match=re.search(rf"\bimpl(?:\s*<[^{{>]*>)?\s+{re.escape(type_name)}\b", clean)
    if not match: fail(f"missing Rust impl: {type_name}")
    body=block(clean, match.group(0))
    return sorted(re.findall(r"\bpub\s+(?:const\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(", body))

def feature_body(cargo, name):
    match=re.search(rf"(?ms)^\s*{re.escape(name)}\s*=\s*\[(.*?)\]", cargo)
    return match.group(1) if match else ""

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--paths", nargs="*", type=Path, default=[])
    parser.add_argument("--json", action="store_true", required=True)
    args=parser.parse_args()
    root=args.root.resolve(); config=json.loads(args.config.read_text(encoding="utf-8"))
    selected=[]
    for value in args.paths:
        relative, path=normalized(root, value); selected.append(relative)
        if not path.exists(): fail(f"selected path missing: {relative}")
    sources={}
    hashes={}
    for key, relative in sorted(config["paths"].items()):
        normalized_name, path=normalized(root, Path(relative))
        data=path.read_bytes(); hashes[normalized_name]=hashlib.sha256(data).hexdigest()
        sources[key]=data.decode("utf-8")
    authority=sources["submit_authority"]; submit_lib=sources["submit_lib"]
    cli=sources["cli_source"]; cli_lib=sources["cli_lib"]
    cli_cargo=sources["cli_cargo"]; submit_cargo=sources["submit_cargo"]
    clean_cli=strip_rust(cli); t4b=block(clean_cli, "mod t4b_shadow")

    view=methods(authority, "CheckedBindingsView")
    nested={name: methods(authority, name) for name in config["nested_getters"]}
    grouped=re.search(r'(?s)#\[cfg\(feature = "t4b-shadow"\)\]\s*pub use mev_trader::\{(.*?)\};', cli_lib)
    exports=sorted(re.findall(r"\b[A-Z][A-Za-z0-9_]*\b", grouped.group(1))) if grouped else []
    adapter=block(clean_cli, "impl<Provider> CandidateExecutionAdapter")
    audited=block(clean_cli, "impl<DB: Database> Database for AuditedDatabase")
    commit=block(clean_cli, "impl<DB: DatabaseCommit> DatabaseCommit for AuditedDatabase")
    observer=block(clean_cli, "impl<Provider> CandidateTxShapeObserver for T4bShadowAuthority")
    observe=block(observer, "fn try_observe")
    statement=re.search(r"\{\s*(?:let\s+pre\s*=\s*match\s+)?([^;\n]+)", observe)
    first=statement.group(1).strip() if statement else ""

    forbidden_names=["send_gated", "RawEgress", "RawBackend", "ProdBackend", "SigningKey",
                     "HotWalletKey", "reqwest", "k256", "rand_08", "transport", "key_loader",
                     "capture_priority_economics_fixture", "PriorityEconomicsCapture"]
    forbidden=[]
    for name in forbidden_names:
        if re.search(rf"\b{re.escape(name)}\b", t4b): forbidden.append(name)
    raw_escapes=[]
    if re.search(r"\bpub\s+(?:const\s+)?fn\s+(?:raw|raw_tx|raw_bytes)\b", authority): raw_escapes.append("raw getter")
    if re.search(r"derive\s*\([^)]*(?:Serialize|Deserialize)[^)]*\)", authority): raw_escapes.append("serde owner")
    if re.search(r"pub\s+use\s+tx_authority::\{?[^;}]*Raw", submit_lib): raw_escapes.append("raw root reexport")
    if re.search(r"pub\s+struct\s+\w*Raw\w*\s*\{", authority): raw_escapes.append("raw wrapper")
    normal_features=feature_body(cli_cargo, "t4b-shadow") + feature_body(submit_cargo, "tx-authority")
    for name in ["phase-b", "arm", "egress", "reqwest", "k256", "rand_08", "signer", "transport", "capture"]:
        if name in normal_features: forbidden.append(f"feature:{name}")

    positives={
      "submit-linear-chain": all(x in authority + cli for x in ["pub trait CandidateExecutionAdapter", "pub fn execute_once", "request.into_parts()", "parts.into_tx_and_bindings()"]),
      "checked-bindings-view-exact": view == sorted(config["checked_bindings_view"]),
      "nested-getters-exact": nested == {k: sorted(v) for k,v in config["nested_getters"].items()},
      "cli-root-exports-exact": exports == sorted(config["cli_root_exports"]),
      "audited-database-five-plus-commit": sorted(re.findall(r"\bfn\s+(basic|block_hash|code_by_hash|storage|storage_by_account_id)\s*\(", audited)) == ["basic","block_hash","code_by_hash","storage","storage_by_account_id"] and len(re.findall(r"\bfn\s+commit\s*\(", commit)) == 1,
      "candidate-blockhash-reject": "AuditPhase::Candidate" in audited and "CandidateBlockHashForbidden" in audited,
      "single-concrete-evm-transact": len(re.findall(r"\bevm\s*\.\s*transact\s*\(", adapter)) == 1,
      "production-chain": "&CandidateAssemblyView<'_>" in observe and "prepare_pre_economics(view)" in first and len(re.findall(r"T4bParentOverlayAdapter\s*::\s*new\s*\(", observe)) == 1 and len(re.findall(r"\.\s*execute_once\s*\(", observe)) == 1,
    }
    counts={
      "optional-reqwest-dependency": len(re.findall(r"(?m)^reqwest\s*=.*optional\s*=\s*true", cli_cargo)),
      "mutant-feature": len(re.findall(r"(?m)^t4b-mutant-egress\s*=", cli_cargo)),
      "feature-dependency-edge": len(re.findall(r't4b-mutant-egress[^\n]*dep:reqwest', cli_cargo)),
      "blocking-reqwest-import": len(re.findall(r"use\s+reqwest::blocking::Client", cli)),
      "public-egress-probe": len(re.findall(r"pub\s+struct\s+T4bMutantEgressProbe", cli)),
      "egress-send-method": len(re.findall(r"impl\s+T4bMutantEgressProbe[\s\S]*?pub\s+fn\s+send\s*\(", cli)),
      "loopback-url": cli.count("http://127.0.0.1:9/gjc-t4b-mutant-egress"),
      "exact-egress-body": cli.count('.body("gjc-t4b-mutant-egress")'),
      "observer-first-statement-call": int(
          "T4bMutantEgressProbe::send()" in observe
          and observe.index("T4bMutantEgressProbe::send()")
              < observe.index("prepare_pre_economics(view)")
      ),
      "root-probe-reexport": len(re.findall(r"pub\s+use\s+mev_trader::T4bMutantEgressProbe", cli_lib)),
    }
    named={"egress-sender": int(all(counts[name] == 1 for name in TEN))}
    passed=all(positives.values()) and not forbidden and not raw_escapes and named["egress-sender"] == 0
    result={"schema_version":SCHEMA,"root":".","selected_paths":sorted(selected),
            "source_sha256":hashlib.sha256("".join(f"{k}\0{hashes[k]}\n" for k in sorted(hashes)).encode()).hexdigest(),
            "files":hashes,"positive_controls":positives,"forbidden_controls":sorted(set(forbidden+raw_escapes)),
            "counts":counts,"named_red":named,"pass":passed}
    print(json.dumps(result,sort_keys=True,separators=(",",":")))
    raise SystemExit(0 if passed else 1)
if __name__ == "__main__": main()
'''

VERIFY = r'''#!/usr/bin/env python3
"""Verify an immutable detector bundle without network access."""
from __future__ import annotations
import argparse, hashlib, json
from pathlib import Path

def digest(data): return hashlib.sha256(data).hexdigest()
def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--bundle", type=Path, required=True)
    mode=parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--print-sha256", action="store_true")
    mode.add_argument("--expected-sha256")
    parser.add_argument("--json", action="store_true", required=True)
    args=parser.parse_args(); bundle=args.bundle.resolve()
    manifest_bytes=(bundle/"manifest.json").read_bytes(); actual=digest(manifest_bytes)
    recorded=(bundle/"MANIFEST.sha256").read_text(encoding="ascii").strip()
    manifest=json.loads(manifest_bytes)
    errors=[]
    if actual != recorded: errors.append("recorded manifest SHA mismatch")
    if args.expected_sha256 and actual != args.expected_sha256.lower(): errors.append("expected manifest SHA mismatch")
    expected_names=set(manifest["files"]) | {"manifest.json", "MANIFEST.sha256"}
    actual_names={path.name for path in bundle.iterdir()}
    if actual_names != expected_names: errors.append("bundle file inventory mismatch")
    for name, expected in sorted(manifest["files"].items()):
        unresolved=bundle/name
        if unresolved.is_symlink(): errors.append(f"bundle symlink forbidden: {name}"); continue
        path=unresolved.resolve()
        try: path.relative_to(bundle)
        except ValueError: errors.append(f"manifest path escapes bundle: {name}"); continue
        if not path.is_file() or digest(path.read_bytes()) != expected: errors.append(f"file SHA mismatch: {name}")
    result={"schema_version":"t4b-capability-detector-verify/v1","bundle":".","manifest_sha256":actual,"verified":not errors,"errors":errors}
    print(json.dumps(result,sort_keys=True,separators=(",",":")))
    raise SystemExit(0 if not errors else 1)
if __name__ == "__main__": main()
'''

def encoded(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()

def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--create-new", action="store_true", required=True)
    parser.add_argument("--json", action="store_true", required=True)
    args = parser.parse_args()
    output = args.output.expanduser().resolve()
    if output.exists():
        raise SystemExit(f"refusing to replace immutable bundle: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    payloads = {
        "scan-config.json": encoded(CONFIG),
        "scan.py": SCAN.encode(),
        "verify.py": VERIFY.encode(),
    }
    manifest = encoded({"schema_version": SCHEMA, "files": {name: sha(data) for name, data in sorted(payloads.items())}})
    payloads["manifest.json"] = manifest
    payloads["MANIFEST.sha256"] = (sha(manifest) + "\n").encode()
    temporary = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent))
    try:
        for name, data in payloads.items():
            path = temporary / name
            path.write_bytes(data)
            path.chmod(stat.S_IRUSR | stat.S_IRGRP | stat.S_IROTH | (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH if name.endswith(".py") else 0))
        temporary.chmod(stat.S_IRUSR | stat.S_IXUSR | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH)
        os.rename(temporary, output)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise
    print(json.dumps({"schema_version": SCHEMA, "bundle": str(output), "manifest_sha256": sha(manifest)}, sort_keys=True, separators=(",", ":")))

if __name__ == "__main__":
    main()
