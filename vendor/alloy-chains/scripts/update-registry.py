#!/usr/bin/env python3
"""Update generated chain registry artifacts."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from string import Template


ROOT = Path(__file__).resolve().parents[1]
INLINE_FLAG_CHAIN_COUNT = 2
MAX_U8_FLAGS = 8


class RustTemplate(Template):
    delimiter = "%%"


@dataclass(frozen=True)
class ChainFlag:
    name: str
    alias: str
    predicate: str
    attr: str | None = None
    tag: str | None = None


BASE_CHAIN_FLAGS = (
    ChainFlag("FLAG_LEGACY", "L", "legacy", attr="is_legacy"),
    ChainFlag("FLAG_SUPPORTS_SHANGHAI", "S", "supports_shanghai", attr="supports_shanghai"),
    ChainFlag("FLAG_TESTNET", "T", "testnet", attr="is_testnet"),
)
TAG_CHAIN_FLAGS = (
    ChainFlag("FLAG_ETHEREUM", "E", "ethereum", tag="ethereum"),
    ChainFlag("FLAG_OPTIMISM", "O", "optimism", tag="optimism"),
    ChainFlag("FLAG_GNOSIS", "G", "gnosis", tag="gnosis"),
    ChainFlag("FLAG_POLYGON", "P", "polygon", tag="polygon"),
    ChainFlag("FLAG_ARBITRUM", "A", "arbitrum", tag="arbitrum"),
    ChainFlag("FLAG_ELASTIC", "X", "elastic", tag="elastic"),
    ChainFlag("FLAG_TEMPO", "TP", "tempo", tag="tempo"),
    ChainFlag("FLAG_CUSTOM_SOURCIFY", "CS", "custom_sourcify", tag="custom-sourcify"),
)
CHAIN_FLAGS = (*BASE_CHAIN_FLAGS, *TAG_CHAIN_FLAGS)


@dataclass(frozen=True)
class Chain:
    chain_id: int
    internal_id: str
    name: str
    aliases: tuple[str, ...]
    serde_name: str | None
    serde_aliases: tuple[str, ...]
    average_blocktime_hint: int | None
    is_legacy: bool
    supports_shanghai: bool
    is_testnet: bool
    native_currency_symbol: str | None
    etherscan_api_url: str | None
    etherscan_base_url: str | None
    etherscan_api_key_name: str | None
    tags: frozenset[str]
    wrapped_native_token: str | None


class StaticStringTable:
    def __init__(self):
        self.values: list[str] = []
        self.indexes: dict[str, int] = {}

    def add(self, value: str | None) -> str:
        if value is None:
            return "N"
        if not value:
            raise ValueError("Static string cannot be empty")

        index = self.indexes.get(value)
        if index is None:
            index = len(self.values)
            if index >= 0xFF:
                raise ValueError("Too many static strings for compact storage")
            self.values.append(value)
            self.indexes[value] = index

        return str(index)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manual", type=Path, default=ROOT / "registry" / "manual.json")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    manual = load_json(args.manual)
    chains = load_manual_chains(manual)
    validate_chains(chains)

    outputs = {
        ROOT / "assets" / "chains.json": json_dump(asset_chains(chains)),
        ROOT / "src" / "generated" / "mod.rs": format_rust(generated_mod()),
        ROOT / "src" / "generated" / "named.rs": format_rust(generated_named(chains)),
    }

    changed = [path for path, contents in outputs.items() if read_text(path) != contents]
    if args.check:
        if changed:
            for path in changed:
                print(f"{path.relative_to(ROOT)} is not up to date", file=sys.stderr)
            return 1
        return 0

    for path, contents in outputs.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)
    return 0


def load_json(path: Path):
    return json.loads(path.read_text())


def read_text(path: Path) -> str | None:
    try:
        return path.read_text()
    except FileNotFoundError:
        return None


def load_manual_chains(manual: dict) -> list[Chain]:
    chains = []
    for raw in manual["chains"]:
        chain_id = raw["chainId"]
        chains.append(
            Chain(
                chain_id=chain_id,
                internal_id=raw["internalId"],
                name=raw["name"],
                aliases=tuple(raw.get("aliases", [])),
                serde_name=raw.get("serdeName"),
                serde_aliases=tuple(raw.get("serdeAliases", [])),
                average_blocktime_hint=raw.get("averageBlocktimeHint"),
                is_legacy=raw.get("isLegacy", False),
                supports_shanghai=raw.get("supportsShanghai", False),
                is_testnet=raw.get("isTestnet", False),
                native_currency_symbol=raw.get("nativeCurrencySymbol"),
                etherscan_api_url=raw.get("etherscanApiUrl"),
                etherscan_base_url=raw.get("etherscanBaseUrl"),
                etherscan_api_key_name=raw.get("etherscanApiKeyName"),
                tags=frozenset(raw.get("tags", [])),
                wrapped_native_token=raw.get("wrappedNativeToken"),
            )
        )
    return chains


def validate_chains(chains: list[Chain]) -> None:
    seen_ids = set()
    seen_variants = set()
    parse_names = {}
    serde_names = {}

    for chain in chains:
        if chain.chain_id in seen_ids:
            raise ValueError(f"Duplicate chain ID {chain.chain_id}")
        if chain.internal_id in seen_variants:
            raise ValueError(f"Duplicate internal ID {chain.internal_id}")
        seen_ids.add(chain.chain_id)
        seen_variants.add(chain.internal_id)

        if (chain.etherscan_api_url is None) != (chain.etherscan_base_url is None):
            raise ValueError(
                f"Chain {chain.internal_id} must define both etherscanApiUrl and etherscanBaseUrl"
            )

        for name in [chain.name, *chain.aliases]:
            previous = parse_names.setdefault(name, chain.internal_id)
            if previous != chain.internal_id:
                raise ValueError(f"Parse name {name!r} is used by {previous} and {chain.internal_id}")

        for name in serde_parse_names(chain):
            previous = serde_names.setdefault(name, chain.internal_id)
            if previous != chain.internal_id:
                raise ValueError(f"Serde name {name!r} is used by {previous} and {chain.internal_id}")


def asset_chains(chains: list[Chain]) -> dict:
    return {
        "chains": {
            str(chain.chain_id): {
                "internalId": chain.internal_id,
                "name": chain.name,
                "averageBlocktimeHint": chain.average_blocktime_hint,
                "isLegacy": chain.is_legacy,
                "supportsShanghai": chain.supports_shanghai,
                "isTestnet": chain.is_testnet,
                "nativeCurrencySymbol": chain.native_currency_symbol,
                "etherscanApiUrl": chain.etherscan_api_url,
                "etherscanBaseUrl": chain.etherscan_base_url,
                "etherscanApiKeyName": chain.etherscan_api_key_name,
            }
            for chain in sorted(chains, key=lambda chain: chain.chain_id)
        }
    }


def generated_mod() -> str:
    return render_template(ROOT / "scripts" / "templates" / "generated_mod.rs")


def generated_named(chains: list[Chain]) -> str:
    chains = sorted(chains, key=lambda chain: chain.chain_id)
    if len(chains) > 0xFFFF:
        raise ValueError("Too many chains for compact index storage")
    if len(chains) > 0xFF:
        chain_index_type = "u16"
    else:
        chain_index_type = "u8"

    string_tables = {
        "CHAIN_NAMES": StaticStringTable(),
        "NATIVE_CURRENCY_SYMBOLS": StaticStringTable(),
        "ETHERSCAN_API_URLS": StaticStringTable(),
        "ETHERSCAN_BASE_URLS": StaticStringTable(),
        "ETHERSCAN_API_KEY_NAMES": StaticStringTable(),
    }
    wrapped_native_tokens = unique(
        [chain.wrapped_native_token for chain in chains if chain.wrapped_native_token is not None]
    )
    if len(wrapped_native_tokens) > 0xFF:
        raise ValueError("Too many wrapped native tokens for compact storage")
    wrapped_native_token_indexes = {token: index for index, token in enumerate(wrapped_native_tokens)}

    enum_variants = "\n".join(f"    {chain.internal_id} = {chain.chain_id}," for chain in chains)
    variants = "\n".join(f"        Self::{chain.internal_id}," for chain in chains)
    chain_id_arms = "\n".join(
        f"            {chain.chain_id} => Some(Self::{chain.internal_id})," for chain in chains
    )
    chain_index_arms = "\n".join(
        f"            Self::{chain.internal_id} => {index},"
        for index, chain in enumerate(chains)
    )
    stored_flags = stored_chain_flags(chains)
    flag_type, flag_consts = generated_flag_consts(stored_flags)
    flag_aliases = generated_flag_aliases(stored_flags)
    chain_data = "\n".join(
        "        d("
        f"{chain_string_indexes(string_tables, chain)}, "
        f"{average_blocktime_millis(chain)}, "
        f"{chain_flags(chain, stored_flags)}, "
        f"{wrapped_native_token_index(chain, wrapped_native_token_indexes)}"
        "),"
        for chain in chains
    )
    string_table_data = "\n\n".join(
        static_string_table(name, table) for name, table in string_tables.items()
    )
    wrapped_native_token_data = "\n".join(
        f"    address!({rs_str(token)})," for token in wrapped_native_tokens
    )
    parse_aliases = "\n".join(
        f"    (NamedChain::{chain.internal_id}, {rs_str(alias)}),"
        for chain in chains
        for alias in chain.aliases
    )
    serde_aliases = "\n".join(
        f"    (NamedChain::{chain.internal_id}, {rs_str(alias)}),"
        for chain in chains
        for alias in serde_extra_names(chain)
    )
    parse_entries = [
        (name, chain_index_expr(chain))
        for chain in chains
        for name in parse_names(chain)
    ]
    serde_entries = [
        (name, chain_index_expr(chain))
        for chain in chains
        for name in serde_extra_names(chain)
    ]
    phf_maps = generate_phf_maps(parse_entries, serde_entries)
    chain_data_len = len(chains)
    wrapped_native_token_len = len(wrapped_native_tokens)
    flag_predicates = {
        flag.predicate: flag_predicate_expr(chains, stored_flags, flag, "self")
        for flag in CHAIN_FLAGS
    }

    return render_template(
        ROOT / "scripts" / "templates" / "generated_named.rs",
        chain_data=chain_data,
        chain_data_len=chain_data_len,
        chain_id_arms=chain_id_arms,
        chain_index_arms=chain_index_arms,
        chain_index_type=chain_index_type,
        custom_sourcify_predicate=flag_predicates["custom_sourcify"],
        elastic_predicate=flag_predicates["elastic"],
        enum_variants=enum_variants,
        ethereum_predicate=flag_predicates["ethereum"],
        flag_aliases=flag_aliases,
        flag_consts=flag_consts,
        flag_type=flag_type,
        gnosis_predicate=flag_predicates["gnosis"],
        legacy_predicate=flag_predicates["legacy"],
        optimism_predicate=flag_predicates["optimism"],
        parse_aliases=parse_aliases,
        phf_maps=phf_maps,
        polygon_predicate=flag_predicates["polygon"],
        arbitrum_predicate=flag_predicates["arbitrum"],
        serde_aliases=serde_aliases,
        string_table_data=string_table_data,
        shanghai_predicate=flag_predicates["supports_shanghai"],
        tempo_predicate=flag_predicates["tempo"],
        testnet_predicate=flag_predicates["testnet"],
        variants=variants,
        wrapped_native_token_data=wrapped_native_token_data,
        wrapped_native_token_len=wrapped_native_token_len,
    )


def render_template(path: Path, **values) -> str:
    return RustTemplate(path.read_text()).substitute(**values)


def average_blocktime_millis(chain: Chain) -> int:
    value = chain.average_blocktime_hint or 0
    if value > 0xFFFF:
        raise ValueError(f"{chain.internal_id} average blocktime is too large for compact storage")
    return value


def stored_chain_flags(chains: list[Chain]) -> list[ChainFlag]:
    counts = {flag: len(chains_with_flag(chains, flag)) for flag in CHAIN_FLAGS}
    candidates = [flag for flag in CHAIN_FLAGS if counts[flag] > INLINE_FLAG_CHAIN_COUNT]
    if len(candidates) <= MAX_U8_FLAGS:
        return candidates

    stored = set(sorted(candidates, key=lambda flag: counts[flag], reverse=True)[:MAX_U8_FLAGS])
    return [flag for flag in CHAIN_FLAGS if flag in stored]


def generated_flag_consts(stored_flags: list[ChainFlag]) -> tuple[str, str]:
    if len(stored_flags) <= 8:
        flag_type = "u8"
    elif len(stored_flags) <= 16:
        flag_type = "u16"
    elif len(stored_flags) <= 32:
        flag_type = "u32"
    else:
        raise ValueError("Too many chain flags for compact storage")

    return flag_type, "\n".join(
        f"const {flag.name}: ChainFlags = 1 << {index};"
        for index, flag in enumerate(stored_flags)
    )


def generated_flag_aliases(stored_flags: list[ChainFlag]) -> str:
    return "\n".join(
        f"    use {flag.name} as {flag.alias};"
        for flag in stored_flags
    )


def flag_predicate_expr(
    chains: list[Chain],
    stored_flags: list[ChainFlag],
    flag: ChainFlag,
    receiver: str,
) -> str:
    if flag in stored_flags:
        return f"{receiver}.has_flag({flag.name})"

    return matches_expr(chains_with_flag(chains, flag), receiver)


def matches_expr(chains: list[Chain], receiver: str) -> str:
    if not chains:
        return "false"
    variants = " | ".join(f"Self::{chain.internal_id}" for chain in chains)
    return f"matches!({receiver}, {variants})"


def chains_with_flag(chains: list[Chain], flag: ChainFlag) -> list[Chain]:
    return [chain for chain in chains if chain_has_flag(chain, flag)]


def chain_flags(chain: Chain, stored_flags: list[ChainFlag]) -> str:
    flags = [flag.alias for flag in stored_flags if chain_has_flag(chain, flag)]
    return " | ".join(flags) if flags else "0"


def chain_index_expr(chain: Chain) -> str:
    return f"NamedChain::{chain.internal_id}.index()"


def chain_has_flag(chain: Chain, flag: ChainFlag) -> bool:
    if flag.attr is not None:
        return bool(getattr(chain, flag.attr))
    if flag.tag is not None:
        return flag.tag in chain.tags
    raise ValueError(f"Flag {flag.name} has no source")


def chain_string_indexes(string_tables: dict[str, StaticStringTable], chain: Chain) -> str:
    indexes = (
        string_tables["CHAIN_NAMES"].add(chain.name),
        string_tables["NATIVE_CURRENCY_SYMBOLS"].add(chain.native_currency_symbol),
        string_tables["ETHERSCAN_API_URLS"].add(chain.etherscan_api_url),
        string_tables["ETHERSCAN_BASE_URLS"].add(chain.etherscan_base_url),
        string_tables["ETHERSCAN_API_KEY_NAMES"].add(chain.etherscan_api_key_name),
    )
    return ", ".join(indexes)


def static_string_table(name: str, table: StaticStringTable) -> str:
    values = "\n".join(f"    {rs_str(value)}," for value in table.values)
    return f"static {name}: [&str; {len(table.values)}] = [\n{values}\n];"


def wrapped_native_token_index(chain: Chain, indexes: dict[str, int]) -> str:
    if chain.wrapped_native_token is None:
        return "W"
    return str(indexes[chain.wrapped_native_token])


def generate_phf_maps(
    parse_entries: list[tuple[str, str]],
    serde_entries: list[tuple[str, str]],
) -> str:
    lines = ["map\tPARSE_NAMES\t"]
    lines.extend(phf_entry_lines(parse_entries))
    lines.append("end")
    lines.append('map\tSERDE_NAMES\t#[cfg(feature = "serde")]')
    lines.extend(phf_entry_lines(serde_entries))
    lines.append("end")

    output = subprocess.run(
        ["cargo", "-Zscript", str(ROOT / "scripts" / "phf-codegen.rs")],
        input="\n".join(lines) + "\n",
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=True,
    )
    return output.stdout.strip()


def phf_entry_lines(entries: list[tuple[str, str]]) -> list[str]:
    lines = []
    for key, value in entries:
        if "\t" in key or "\n" in key:
            raise ValueError(f"PHF key contains unsupported whitespace: {key!r}")
        lines.append(f"{key}\t{value}")
    return lines


def serde_extra_names(chain: Chain) -> list[str]:
    parse = set(parse_names(chain))
    serde_name = chain.serde_name or snake_case(chain.internal_id)
    return [name for name in unique([serde_name, *chain.serde_aliases]) if name not in parse]


def parse_names(chain: Chain) -> list[str]:
    return unique([chain.name, *chain.aliases])


def serde_parse_names(chain: Chain) -> list[str]:
    serde_name = chain.serde_name or snake_case(chain.internal_id)
    return unique([*parse_names(chain), serde_name, *chain.serde_aliases])


def snake_case(value: str) -> str:
    output = []
    chars = list(value)
    for index, char in enumerate(chars):
        if char.isupper() and index > 0:
            prev = chars[index - 1]
            next_char = chars[index + 1] if index + 1 < len(chars) else ""
            if prev.islower() or prev.isdigit() or (prev.isupper() and next_char.islower()):
                output.append("_")
        output.append(char.lower())
    return "".join(output)


def unique(items: list[str]) -> list[str]:
    seen = set()
    output = []
    for item in items:
        if item not in seen:
            seen.add(item)
            output.append(item)
    return output


def rs_str(value: str) -> str:
    return json.dumps(value)


def json_dump(value) -> str:
    return json.dumps(value, indent=2) + "\n"


def format_rust(source: str) -> str:
    output = subprocess.run(
        ["rustfmt", "--edition", "2024", "--emit", "stdout"],
        input=source,
        text=True,
        capture_output=True,
        check=True,
    )
    return output.stdout


if __name__ == "__main__":
    raise SystemExit(main())
