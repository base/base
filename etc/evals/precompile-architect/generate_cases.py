#!/usr/bin/env python3
"""Generate precompile-architect eval cases into cases/.

Every case definition and hand label lives here. Edit cases here, not in the JSON.

Historical cases take files.create and files.modify straight from the merged diff,
so labels always match what landed. Verdict, approach, surfaces, critical files,
frozen files and rubrics are labelled by hand.

Run from anywhere: python3 etc/evals/precompile-architect/generate_cases.py
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from evallib import CASES, DEFAULT_ALLOWED, diff_name_status, fork_state, full_sha  # noqa: E402

P = "crates/common/precompiles"
S = f"{P}/src"
T = f"{P}/tests"
# Synthetic cases are pinned to this commit so the case set is reproducible. Re-pin deliberately
# (and re-check every synthetic label) when the live fork set changes.
SYNTHETIC_BASE = "fa0f64d32012efa9b0934bfe1dd074a4aed329c7"


def logic(module, *versions):
    return [f"{S}/{module}/logic/{v}.rs" for v in versions]


def golden(name, *versions):
    return [f"{T}/b20_{name}_{v}_golden.rs" for v in versions]


def surfaces(*on):
    return {name: name in on for name in ("gas", "revert_bytes", "storage", "abi", "events")}


def versions(create=(), modify=(), frozen=()):
    return {"create": list(create), "modify": list(modify), "frozen": list(frozen)}


def r(rubric_id, check, critical=False):
    return {"id": rubric_id, "check": check, "critical": critical}


def alt(reason, **fields):
    """An alternative answer experts would also accept, overriding approach, activation_fork or versions."""
    return {"reason": reason, **fields}


REJECT_RUBRIC = r("safe_alternative", "The plan proposes a consensus-safe alternative, such as activating the change at a future fork.")
ALL_SURFACES = ["gas", "revert_bytes", "storage", "abi", "events"]
CASES_DEF = []


def case(case_id, *, ref=None, base=None, category, polarity, split, ticket, verdict="proceed", approach,
         activation_fork, versions_, must_not_modify=(), symmetric=(), surf=(), goldens_unchanged=True,
         tests_new=(), critical, extra_allowed=(), alternatives=(), graded_surfaces=None, create=None,
         modify=None, rubric, notes):
    CASES_DEF.append(locals())


# =============================================================================================
# Era 1: pre-Beryl. Nothing scheduled on any network, so in-place edits are correct.
# =============================================================================================

case("pre-beryl-mint-pause-before-policy", ref="75d746d794",
    category="revert_bytes", polarity="negative", split="dev",
    ticket="A paused B-20 token whose mint receiver is blocked by policy reverts with PolicyForbids on mint. It should revert with ContractPaused, matching transfer. Make the pause check run before the mint-receiver policy check, and add a regression test.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(),
    surf=["revert_bytes"], tests_new=["paused mint with a policy-denied receiver reverts ContractPaused"],
    critical=[f"{S}/common/ops/mintable.rs"],
    rubric=[r("nothing_live", "The plan recognizes that no network has scheduled the upgrade that ships this code, so there is no executed behavior to preserve."),
            r("no_version_gate", "The plan edits the shared mint logic directly and does not introduce a logic version or fork gate.", critical=True)],
    notes="No network has Beryl scheduled. Reordering guards changes revert bytes, but nothing has executed, so editing in place is correct. A version or fork gate is over-engineering.")

case("pre-beryl-transfer-zero-receiver-first", ref="0327b15d89",
    category="revert_bytes", polarity="negative", split="holdout",
    ticket="When both from and to are the zero address, B-20 transfer reverts with InvalidSender. The base-std Solidity reference reverts with InvalidReceiver in that case. Align the precompile with the reference and add a regression test.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(),
    surf=["revert_bytes"], tests_new=["transfer with from == to == address(0) reverts InvalidReceiver"],
    critical=[f"{S}/common/ops/transferable.rs"],
    rubric=[r("nothing_live", "The plan recognizes that no network runs this code yet, so the revert order can change without a fork gate.", critical=True)],
    notes="Pre-Beryl. Swapping guard order changes revert bytes for one input, but nothing has executed it.")

case("pre-beryl-executor-policy-infinite-allowance", ref="cdee452f22",
    category="old_fork_change", polarity="negative", split="dev",
    ticket="transferFrom skips the TRANSFER_EXECUTOR_POLICY check when the spender has an infinite allowance, so a blocked executor with max allowance can still move funds. Enforce the executor policy in transferFrom regardless of allowance, keep skipping the allowance decrement for infinite allowances, and add a regression test.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(),
    surf=["revert_bytes"], tests_new=["transferFrom with max allowance reverts PolicyForbids when the executor policy denies"],
    critical=[f"{S}/common/ops/transferable.rs"],
    rubric=[r("nothing_live", "The plan recognizes that this behavior has not executed on any network, so it can be fixed in place.", critical=True)],
    notes="Pre-Beryl. Looks like changing old behavior, but there is no old behavior yet.")

case("pre-beryl-remove-batch-burn", ref="82e0ebc724",
    category="deprecate_old", polarity="positive", split="holdout",
    ticket="The security token variant should not expose batchBurn or BURN_FROM_ROLE. Remove them from the precompile, including the ABI entries, dispatch arms, token logic and guard, and update every test or harness that calls them.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(),
    symmetric=["b20_security"], surf=["abi"],
    tests_new=["harness tests no longer call batchBurn or BURN_FROM_ROLE"],
    critical=[f"{S}/b20_security/abi.rs", f"{S}/b20_security/dispatch.rs", f"{S}/b20_security/token.rs"],
    extra_allowed=["actions/harness/**"],
    rubric=[r("removal_safe_pre_launch", "The plan states that removing a selector is safe here because no network has activated the precompile.", critical=True),
            r("downstream_harness", "The plan updates the tests or harnesses that reference the removed selector.")],
    notes="Deleting a selector from a live surface would break consensus. Here nothing is scheduled, so deletion is correct.")

case("pre-beryl-native-gas-accounting", ref="ff28a3efc5",
    category="gas_semantics", polarity="positive", split="holdout",
    ticket="Native precompile storage access is charged a flat cost. Charge EIP-2929 warm and cold access costs, EIP-2200 SSTORE costs and EIP-3529 refunds for native precompile storage, the way the EVM charges contracts. Put the accounting where every precompile benefits.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(),
    surf=["gas"], tests_new=["cold and warm sload costs", "sstore costs and refunds"],
    critical=[f"crates/common/precompile-storage/src/evm.rs", f"crates/common/precompile-storage/src/provider.rs"],
    extra_allowed=["crates/common/precompile-storage/src/**", f"{S}/macros.rs"],
    rubric=[r("provider_layer", "The plan puts gas accounting in the storage provider layer rather than in each precompile."),
            r("nothing_live", "The plan recognizes no network runs these precompiles yet, so the gas schedule can change without a fork gate.", critical=True)],
    notes="A sweeping gas change. Pre-Beryl, so in-place is correct.")

# =============================================================================================
# Era 2: Beryl live on zeronet only, a pre-production network.
# =============================================================================================

case("zeronet-factory-keccak-metering", ref="90296bcf0a",
    category="gas_semantics", polarity="positive", split="dev",
    ticket="The B-20 factory derives token addresses with keccak256 in dispatch but charges no gas for the hashing. Meter that keccak work through the storage provider so every provider counts it. Zeronet is a pre-production network that is reset as needed; the team does not preserve zeronet execution.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(),
    symmetric=["b20_factory"], surf=["gas"], tests_new=["address derivation charges keccak gas"],
    critical=[f"{S}/b20_factory/dispatch.rs", "crates/common/precompile-storage/src/provider.rs"],
    extra_allowed=["crates/common/precompile-storage/src/**", f"{S}/b20_factory/**"],
    alternatives=[alt("A fork gate is safe, just unnecessary.", approach="fork_gate")],
    rubric=[r("zeronet_reasoning", "The plan reasons about Beryl being live only on zeronet and concludes an in-place change is acceptable because zeronet execution is not preserved.", critical=True)],
    notes="The ticket states zeronet execution is not preserved so the task is unambiguous.")

case("zeronet-asset-zero-multiplier", ref="6372330f9c",
    category="revert_bytes", polarity="positive", split="dev",
    ticket="updateMultiplier on the asset token accepts zero, which would zero every scaled balance. Reject a zero multiplier with a new typed error added to the asset ABI. Zeronet is a pre-production network that is reset as needed; the team does not preserve zeronet execution.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(),
    symmetric=["b20_asset"], surf=["revert_bytes", "abi"], tests_new=["updateMultiplier(0) reverts with the new error"],
    critical=[f"{S}/b20_asset/abi.rs", f"{S}/b20_asset/token.rs"],
    alternatives=[alt("A fork gate is safe, just unnecessary.", approach="fork_gate")],
    rubric=[r("zeronet_reasoning", "The plan concludes an in-place change is acceptable because only zeronet runs Beryl and zeronet execution is not preserved.", critical=True),
            r("typed_error", "The plan adds a new typed error to the asset ABI for the zero multiplier.")],
    notes="Only zeronet has Beryl live, and the ticket says zeronet is not preserved.")

# =============================================================================================
# Era 3: Beryl live on every network. V1 is frozen.
# =============================================================================================

case("beryl-solidify-stablecoin", ref="401ffe4d60",
    category="scaffold_logic", polarity="positive", split="dev",
    ticket="Stablecoin token logic lives in token.rs as capability-trait impls shared across forks, so any future edit there changes Beryl execution. Restructure the stablecoin precompile so each fork's logic is frozen in its own versioned file and the dispatcher routes by the active hardfork. Behavior, gas and revert bytes must stay identical.",
    approach="edit_in_place", activation_fork=None, versions_=versions(create=["V1"]),
    symmetric=["b20_stablecoin"], tests_new=["Azul resolves to no version and Beryl to V1", "V1 unit tests against local fakes"],
    critical=[f"{S}/b20_stablecoin/versions.rs", f"{S}/b20_stablecoin/logic/v1.rs", f"{S}/b20_stablecoin/logic/interface.rs", f"{S}/b20_stablecoin/dispatch.rs"],
    extra_allowed=[f"{S}/b20_stablecoin/**", f"{S}/lookup.rs", f"{S}/provider.rs"],
    must_not_modify=[f"{S}/b20_stablecoin/abi.rs", f"{S}/b20_stablecoin/storage.rs"],
    rubric=[r("behavior_identical", "The plan requires V1 to reproduce current Beryl behavior exactly, including revert order and storage access order.", critical=True),
            r("version_manager", "The plan adds a version resolver that owns fork routing, and an append-only logic interface.")],
    notes="Pure restructure. Execution at every fork is unchanged, so activation_fork is null and approach is edit_in_place even though a V1 file is created.")

case("cobalt-scaffold-policy-v2", ref="2c99c12cce",
    category="scaffold_logic", polarity="positive", split="dev",
    ticket="Cobalt will add composite policies to the policy registry. Prepare the policy registry so Cobalt-era changes can land without affecting Beryl execution. This ticket must not change any behavior.",
    approach="new_logic_version", activation_fork="Cobalt", versions_=versions(create=["V2"], frozen=["V1"]),
    must_not_modify=logic("policy", "v1"), symmetric=["policy"],
    tests_new=["policy V2 golden at Cobalt reproduces V1", "Cobalt resolves to V2 and Beryl to V1"],
    critical=[f"{S}/policy/logic/v2.rs", f"{S}/policy/versions.rs"],
    rubric=[r("copy_of_v1", "The plan makes V2 a behavior-identical copy of V1.", critical=True),
            r("v1_frozen_reason", "The plan explains V1 must not change because Beryl is live.")],
    notes="The V1 golden edit only drops a line asserting Cobalt resolves to V1; pinned roots are unchanged.")

case("cobalt-scaffold-stablecoin-v2", ref="a16c5a6396",
    category="scaffold_logic", polarity="positive", split="holdout",
    ticket="Cobalt will add seize support to the stablecoin token. Prepare the stablecoin precompile so Cobalt-era changes can land without affecting Beryl execution. This ticket must not change any behavior.",
    approach="new_logic_version", activation_fork="Cobalt", versions_=versions(create=["V2"], frozen=["V1"]),
    must_not_modify=logic("b20_stablecoin", "v1"), symmetric=["b20_stablecoin"],
    tests_new=["Cobalt resolves to V2 and Beryl to V1"],
    critical=[f"{S}/b20_stablecoin/logic/v2.rs", f"{S}/b20_stablecoin/versions.rs"],
    rubric=[r("copy_of_v1", "The plan makes V2 a behavior-identical copy of V1.", critical=True)],
    notes="Positive scaffold case added for balance.")

case("cobalt-scaffold-asset-v2-erc8056-surface", ref="64c1402ba8",
    category="scaffold_logic_abi", polarity="positive", split="dev",
    ticket="Cobalt adds the ERC-8056 scheduled UI multiplier to the asset token: new view and setter selectors, errors, events, and storage for a pending multiplier. Scaffold the Cobalt asset version with that wire and storage surface; the behavior itself lands in a later ticket. Beryl execution must be unchanged, including what the new selectors return on Beryl.",
    approach="new_logic_version", activation_fork="Cobalt", versions_=versions(create=["V2"], frozen=["V1"]),
    must_not_modify=logic("b20_asset", "v1"), symmetric=["b20_asset"], surf=["storage", "abi"],
    tests_new=["new selectors revert on V1 as unknown selectors", "Cobalt resolves to V2"],
    critical=[f"{S}/b20_asset/logic/v2.rs", f"{S}/b20_asset/versions.rs", f"{S}/b20_asset/abi.rs", f"{S}/b20_asset/storage.rs"],
    extra_allowed=[f"{S}/b20_asset/**", "crates/common/precompile-macros/src/**"],
    rubric=[r("append_only_storage", "The plan appends new storage fields without moving existing slots."),
            r("frozen_selector_default", "The plan makes the new selectors revert on V1 exactly as an unknown selector did before.", critical=True)],
    notes="Logic, ABI and storage scaffold. The new selectors still revert at Cobalt after this change, so revert bytes and emitted events are unchanged; new event definitions count under abi.")

case("cobalt-storage-tail-cleanup", ref="f39d4d201f",
    category="gas_semantics", polarity="positive", split="dev",
    ticket="When a dynamic storage value such as a string shrinks, precompile storage leaves stale tail chunks behind. Clear those chunks on shrink, for every precompile. Beryl blocks must replay with identical storage writes and gas.",
    approach="fork_gate", activation_fork="Cobalt", versions_=versions(),
    surf=["gas", "storage"], tests_new=["shrinking a dynamic value clears tail slots under Cobalt storage features", "tails stay untouched under Legacy features"],
    critical=["crates/common/precompile-storage/src/provider.rs", "crates/common/precompile-storage/src/types/bytes_like.rs"],
    extra_allowed=["crates/common/precompile-storage/src/**", f"{S}/*/precompile.rs", f"{S}/macros.rs", f"{S}/provider.rs", f"{S}/spec.rs"],
    rubric=[r("storage_layer_gate", "The plan gates the cleanup in the storage layer by fork rather than in each precompile's logic version."),
            r("legacy_identical", "The plan keeps Beryl writes and gas identical.", critical=True)],
    notes="Cross-cutting fork gate through storage features. Positive gas/storage case where a gate is required.")

case("cobalt-implement-erc8056-in-v2", ref="b2d5a86fee",
    category="scaffold_logic_abi", polarity="negative", split="holdout",
    ticket="Implement the ERC-8056 scheduled multiplier behavior in the asset token: setUIMultiplier, cancelScheduledMultiplier and the UI-scaled views. The selectors, errors, events and storage already exist on the Cobalt surface.",
    approach="edit_in_place", activation_fork="Cobalt", versions_=versions(modify=["V2"], frozen=["V1"]),
    must_not_modify=logic("b20_asset", "v1"), symmetric=["b20_asset"], surf=["revert_bytes", "storage", "events"],
    tests_new=["V2 unit tests for scheduling and cancelling"],
    critical=[f"{S}/b20_asset/logic/v2.rs"], extra_allowed=[f"{S}/b20_asset/**", "crates/common/precompile-macros/src/**"],
    rubric=[r("edit_v2_in_place", "The plan edits the existing V2 because Cobalt is not scheduled on any network, and does not create V3.", critical=True),
            r("v1_signatures_frozen", "The plan avoids changing V1 code or trait signatures that V1 depends on.")],
    notes="V2 already exists and Cobalt is unscheduled, so a new version is wrong. Under the surfaces convention, Cobalt previously ran the V2 scaffold where these selectors reverted; now they succeed, so revert bytes change, and the setters write storage and emit events.")

case("cobalt-version-gate-seize-scopes", ref="26ca32b580",
    category="old_fork_change", polarity="positive", split="dev",
    ticket="Seize policy scopes were added for Cobalt, but on Beryl tokens updatePolicy and policyId now accept SEIZE_HOLDER_POLICY and SEIZE_RECEIVER_POLICY instead of reverting UnsupportedPolicyType. The base-std v1.0.0 reference, which is what Beryl tokens executed, rejects them. Fix this for asset and stablecoin.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(modify=["V1", "V2"]),
    must_not_modify=[f"{S}/common/policy_type.rs"], symmetric=["b20_asset", "b20_stablecoin"], surf=["revert_bytes"],
    tests_new=["V1 golden: updatePolicy and policyId reject seize scopes"],
    critical=logic("b20_asset", "v1") + logic("b20_stablecoin", "v1"),
    alternatives=[alt("Editing only V1 is equally correct; V2 already accepts the seize scopes, so moving its scope list is optional.", versions=versions(modify=["V1"]))],
    rubric=[r("root_cause_shared", "The plan identifies the shared, unversioned policy-type lookup as the root cause."),
            r("restore_framing", "The plan frames the V1 edit as restoring the behavior Beryl tokens actually executed.", critical=True),
            r("both_modules", "The plan fixes asset and stablecoin symmetrically.", critical=True)],
    notes="Editing live V1 is correct: the shared enum leaked Cobalt scopes into V1 after Beryl shipped. activation_fork is Beryl because the current code at Beryl changes. The setter path now reverts, which is a revert-bytes change only.")

case("beryl-restore-stablecoin-update-policy-order", ref="77d4ff6270",
    category="old_fork_change", polarity="positive", split="holdout",
    ticket="Stablecoin V1 updatePolicy checks policyExists(newPolicyId) before reading the old policyId. The Beryl reference that networks executed reads the old policyId first, so the invalid-new-ID path now spends one fewer SLOAD than canonical. Fix the divergence and pin it with a test.",
    approach="edit_in_place", activation_fork="Beryl", versions_=versions(modify=["V1"]),
    must_not_modify=logic("b20_stablecoin", "v2"), symmetric=["b20_stablecoin"], surf=["gas"],
    tests_new=["V1 golden gas footprint pins the old-policy SLOAD on the revert path"],
    critical=logic("b20_stablecoin", "v1"),
    rubric=[r("canonical_reference", "The plan restores V1 to the behavior Beryl networks executed.", critical=True),
            r("gas_on_revert", "The plan explains that gas spent before a revert is consensus-visible.")],
    notes="This commit reverted an earlier merged reorder (c257e9fccf) that wrongly claimed Beryl was zeronet-only. That earlier commit is deliberately excluded as a reference.")

case("cobalt-composite-policy-type-error", ref="4e6616e266",
    category="revert_bytes", polarity="positive", split="dev",
    ticket="At Cobalt the PolicyType enum gains UNION and INTERSECT. createPolicy and createPolicyWithAccounts with a composite type now reach logic and panic with Panic(0x21). They should revert IncompatiblePolicyType, with the ZeroAddress check taking precedence.",
    approach="edit_in_place", activation_fork="Cobalt", versions_=versions(modify=["V2"], frozen=["V1"]),
    must_not_modify=logic("policy", "v1") + golden("policy", "v1"), symmetric=["policy"], surf=["revert_bytes"],
    tests_new=["V2 golden pins the IncompatiblePolicyType selector"],
    critical=logic("policy", "v2"),
    rubric=[r("v1_unreachable", "The plan notes V1 already rejects composite discriminants at ABI decode and needs no change."),
            r("unshipped", "The plan notes Cobalt is not scheduled, so V2 can change in place.", critical=True)],
    notes="Revert bytes change in unshipped V2 only.")

case("beryl-remove-dead-v1-abi-event", ref="9f8f78e959",
    category="scaffold_logic_abi", polarity="negative", split="dev",
    ticket="The frozen IB20V1 ABI declares a TransferredFromSeizable event that was never part of the Beryl surface and has no emit site. Remove it and update any pinned ABI fingerprint.",
    approach="edit_in_place", activation_fork=None, versions_=versions(frozen=["V1", "V2"]),
    must_not_modify=logic("b20_asset", "v1", "v2") + logic("b20_stablecoin", "v1", "v2"),
    tests_new=["pinned V1 ABI fingerprint updated"],
    critical=[f"{S}/common/abi/v1.rs"],
    rubric=[r("no_execution_change", "The plan argues that removing a never-emitted event cannot change execution, logs or state roots.", critical=True),
            r("no_new_version", "The plan does not create a logic version or fork gate.")],
    notes="Editing a frozen ABI file sounds dangerous, but an unemitted event has no execution effect.")

case("beryl-remove-superseded-capability-traits", ref="9f962a529e",
    category="deprecate_old", polarity="positive", split="dev",
    ticket="The common capability traits Transferable, Mintable, Burnable, Pausable, Configurable, Permittable and RoleManaged predate the versioned logic and have no production callers. Remove them, keep anything still used, and keep every frozen version's production code unchanged.",
    approach="edit_in_place", activation_fork=None, versions_=versions(frozen=["V1", "V2"]),
    tests_new=["each version pins its own EIP-712 domain typehash"],
    critical=[f"{S}/common/ops/transferable.rs", f"{S}/common/ops/mintable.rs"],
    rubric=[r("caller_audit", "The plan verifies there are no production callers before deleting."),
            r("keep_live_shared", "The plan keeps shared code that is still used, such as guards, role ids and permit helpers."),
            r("frozen_prod_untouched", "The plan limits any edit to a frozen logic file to its test module.", critical=True)],
    notes="Frozen logic files change only in test modules, which validate_cases.py enforces.")

case("beryl-borrowed-announce-decode", ref="d6069d2ac0",
    category="revert_bytes", polarity="negative", split="dev",
    ticket="announce(bytes[],string,string,string) decodes into owned buffers, and an aliased payload makes it allocate tens of megabytes per call, a liveness DoS on live Beryl networks. Fix the allocation without changing which inputs are accepted or the bytes any call returns.",
    approach="edit_in_place", activation_fork=None, versions_=versions(frozen=["V1"]),
    must_not_modify=logic("b20_asset", "v1"), symmetric=["b20_asset"],
    tests_new=["borrowed decode accepts a call iff the owned decoder does"],
    critical=[f"{S}/b20_asset/dispatch.rs"],
    rubric=[r("byte_identical", "The plan keeps the accept set and revert bytes identical so it is safe on live Beryl without a fork gate.", critical=True),
            r("no_metering", "The plan avoids size caps or new metering on Beryl because they would change consensus.")],
    notes="The correct plan changes no revert bytes.")

case("cobalt-activation-registry-nonpayable", ref="90e007183d",
    category="revert_bytes", polarity="positive", split="holdout",
    ticket="The activation registry accepts calls that carry ETH value, stranding the ETH, while every other precompile rejects value. Add the nonpayable guard with a NonPayable error, without changing Beryl execution.",
    approach="fork_gate", activation_fork="Cobalt", versions_=versions(),
    symmetric=["activation"], surf=["revert_bytes", "abi"],
    tests_new=["valued call succeeds at Beryl", "valued call reverts NonPayable at Cobalt"],
    critical=[f"{S}/activation/dispatch.rs"], extra_allowed=[f"{S}/activation/**"],
    rubric=[r("fork_gated", "The plan activates the guard at Cobalt so Beryl replay is byte-identical.", critical=True),
            r("both_forks_tested", "The plan tests behavior at both Beryl and Cobalt.")],
    notes="The activation registry has no logic versions, so a fork gate in dispatch is the right approach.")

case("cobalt-meter-permit", ref="32a84239b",
    category="gas_semantics", polarity="positive", split="dev",
    ticket="B-20 permit does EIP-712 hashing and secp256k1 recovery without charging native gas, so a caller can loop permit cheaply. Meter that work for asset and stablecoin without changing Beryl's gas schedule.",
    approach="edit_in_place", activation_fork="Cobalt", versions_=versions(modify=["V2"], frozen=["V1"]),
    symmetric=["b20_asset", "b20_stablecoin"], surf=["gas"], goldens_unchanged=False,
    tests_new=["V2 permit charges recovery gas", "V1 permit charges none"],
    critical=logic("b20_asset", "v2") + logic("b20_stablecoin", "v2") + [f"{S}/common/ops/permit.rs"],
    extra_allowed=["crates/common/precompile-macros/src/**", f"{S}/common/token_accounting.rs"],
    rubric=[r("v1_unmetered", "The plan leaves V1 unmetered because Beryl's gas schedule is frozen.", critical=True),
            r("symmetric", "The plan meters asset and stablecoin identically.")],
    notes="V1 logic files change only in test fakes. A stale V2 golden pin was corrected, so existing_goldens_unchanged is false.")

case("cobalt-zero-address-before-policy-sload", ref="90a7a2a2d",
    category="gas_semantics", polarity="positive", split="holdout",
    ticket="Unprivileged V2 transfer loads policy IDs before checking for a zero from or to address. On a zero-address revert that SLOAD gas is not refunded, diverging from transferFrom and the Cobalt reference. Fix it for asset and stablecoin.",
    approach="edit_in_place", activation_fork="Cobalt", versions_=versions(modify=["V2"], frozen=["V1"]),
    must_not_modify=logic("b20_asset", "v1") + logic("b20_stablecoin", "v1") + golden("asset", "v1") + golden("stablecoin", "v1"),
    symmetric=["b20_asset", "b20_stablecoin"], surf=["gas"],
    tests_new=["V2 golden: zero-address reverts perform no policy SLOAD"],
    critical=logic("b20_asset", "v2") + logic("b20_stablecoin", "v2"),
    rubric=[r("unshipped", "The plan notes Cobalt is not scheduled, so V2 can change in place.", critical=True),
            r("v1_not_affected", "The plan leaves V1 untouched.")],
    notes="Gas-on-revert change in unshipped V2.")

case("cobalt-tristate-announce-decode", ref="a80a17d90",
    category="revert_bytes", polarity="positive", split="dev",
    ticket="A recognized but malformed announce payload still falls through to the owned decoder's expensive diagnostic. Bound the cost of malformed announce calls on Cobalt with a fixed-size error, without changing what Beryl returns.",
    approach="fork_gate", activation_fork="Cobalt", versions_=versions(frozen=["V1"]),
    must_not_modify=logic("b20_asset", "v1"), symmetric=["b20_asset"], surf=["revert_bytes"],
    tests_new=["V1 golden: malformed announce matches the owned decoder", "V2 golden: fixed-size error"],
    critical=[f"{S}/b20_asset/dispatch.rs"],
    alternatives=[alt("The dispatch branch can be described as editing unshipped V2 behavior in place.", approach="edit_in_place", versions=versions(modify=["V2"], frozen=["V1"]))],
    rubric=[r("v1_bytes_frozen", "The plan keeps V1 revert bytes unchanged.", critical=True),
            r("both_forks_tested", "The plan pins both Beryl and Cobalt behavior in tests.")],
    notes="The version split lives in dispatch.")

case("cobalt-selector-only-abi-errors", ref="118dd6849",
    category="revert_bytes", polarity="positive", split="holdout",
    ticket="ABI decode failures return the selector plus a variable-length decoder error string. From Cobalt, return only the 4-byte selector, across every precompile. Beryl revert bytes must not change.",
    approach="fork_gate", activation_fork="Cobalt", versions_=versions(),
    symmetric=["b20_factory", "policy", "nonce", "tx_context"], surf=["revert_bytes"],
    tests_new=["selector plus message under Legacy features", "selector only under Cobalt features"],
    critical=["crates/common/precompile-storage/src/error.rs"],
    extra_allowed=["crates/common/precompile-storage/src/**", f"{S}/*/dispatch.rs", f"{S}/metrics.rs"],
    rubric=[r("central_gate", "The plan gates the encoding centrally rather than per logic version."),
            r("legacy_preserved", "The plan keeps Beryl encoding byte-identical.", critical=True)],
    notes="Cross-cutting fork gate in the error encoding.")

case("cobalt-factory-bootstrap-resolve-version", ref="b2bf3cfa40",
    category="old_fork_change", polarity="positive", split="dev",
    ticket="FactoryV1 routes createB20 init calls through token V1 regardless of fork, so a Cobalt bootstrap init call that depends on a V2-only policy scope reverts and rolls back createB20. Fix it so init calls run against the token version active at the block's fork.",
    approach="edit_in_place", activation_fork="Cobalt", versions_=versions(modify=["V1"]),
    symmetric=["b20_factory"], surf=["revert_bytes"],
    tests_new=["seize-scope init call succeeds at Cobalt", "same call still reverts at Beryl"],
    critical=logic("b20_factory", "v1"),
    rubric=[r("beryl_unchanged", "The plan argues Beryl output is unchanged because the active token version at Beryl is still V1.", critical=True),
            r("paired_tests", "The plan tests Beryl and Cobalt with otherwise identical inputs.")],
    notes="Editing live FactoryV1 is correct because it resolves to the same token version at Beryl. At Cobalt, createB20 calls that reverted now succeed. Under the convention that is a revert-bytes change only, because storage and events flags count only calls that succeed both before and after.")

case("cobalt-zero-copy-create-b20", ref="2488a2cb0",
    category="gas_semantics", polarity="negative", split="holdout",
    ticket="createB20 decodes its bytes[] initCalls argument into owned buffers. Decode it as borrowed slices to avoid the copy, keeping the accepted inputs and every returned byte the same.",
    approach="edit_in_place", activation_fork=None, versions_=versions(frozen=["V1"]),
    symmetric=["b20_factory"], tests_new=["borrowed decode accepts iff owned decode accepts"],
    critical=[f"{S}/b20_factory/dispatch.rs"], extra_allowed=[f"{S}/b20_factory/**"],
    rubric=[r("no_behavior_change", "The plan argues gas and revert bytes are unchanged and adds no fork gate.", critical=True)],
    notes="FactoryV1 is live; its production code changes only in signatures, so behavior is unchanged.")

# =============================================================================================
# Era 4: Cobalt scheduled. V1 and V2 are frozen; Denim work needs V3.
# =============================================================================================

FROZEN_12 = logic("b20_asset", "v1", "v2") + logic("b20_stablecoin", "v1", "v2") + golden("asset", "v1", "v2") + golden("stablecoin", "v1", "v2")

case("denim-scaffold-asset-stablecoin-v3", ref="040c4a80b",
    category="scaffold_logic", polarity="positive", split="dev",
    ticket="Denim will carry new asset and stablecoin token behavior, and several Denim-era transfer and policy changes are queued behind this ticket. Prepare both precompiles so those changes can land without affecting how blocks from earlier forks execute. This ticket must not change any behavior.",
    approach="new_logic_version", activation_fork="Denim", versions_=versions(create=["V3"], frozen=["V1", "V2"]),
    must_not_modify=FROZEN_12, symmetric=["b20_asset", "b20_stablecoin"],
    tests_new=["V3 goldens reproduce V2 at Denim", "Denim resolves to V3"],
    critical=logic("b20_asset", "v3") + logic("b20_stablecoin", "v3") + [f"{S}/b20_asset/versions.rs", f"{S}/b20_stablecoin/versions.rs"],
    extra_allowed=[f"{S}/b20_asset/dispatch.rs", f"{S}/b20_stablecoin/dispatch.rs"],
    rubric=[r("v3_is_copy_of_v2", "The plan makes V3 a behavior-identical copy of V2.", critical=True),
            r("v2_frozen_reason", "The plan explains V2 must not change because Cobalt is scheduled on Sepolia.")],
    notes="V2 is frozen because Cobalt is scheduled on Sepolia.")

case("denim-scaffold-policy-v3", ref="c8520c03f",
    category="scaffold_logic", polarity="positive", split="holdout",
    ticket="Denim will change policy registry behavior. Prepare the policy registry so Denim changes can land without touching Beryl or Cobalt execution. This ticket must not change any behavior.",
    approach="new_logic_version", activation_fork="Denim", versions_=versions(create=["V3"], frozen=["V1", "V2"]),
    must_not_modify=logic("policy", "v1", "v2") + golden("policy", "v1", "v2"), symmetric=["policy"],
    tests_new=["policy V3 golden reproduces V2", "Denim resolves to V3"],
    critical=logic("policy", "v3") + [f"{S}/policy/versions.rs"],
    rubric=[r("copy_of_v2", "The plan copies V2, not V1.", critical=True)],
    notes="Same shape as the asset and stablecoin Denim scaffold.")

case("denim-executor-policy-in-v3", ref="8478f7f16",
    category="scaffold_logic", polarity="negative", split="dev",
    ticket="At Denim, transfer should check TRANSFER_EXECUTOR_POLICY against the caller on every unprivileged transfer, before the sender and receiver checks. Skip the sender check when the caller is the sender and the executor and sender policies match. Apply to asset and stablecoin.",
    approach="edit_in_place", activation_fork="Denim", versions_=versions(modify=["V3"], frozen=["V1", "V2"]),
    must_not_modify=FROZEN_12, symmetric=["b20_asset", "b20_stablecoin"],
    tests_new=["executor checked before sender and receiver", "sender check skipped when executor equals sender"],
    surf=["revert_bytes", "gas"],
    critical=logic("b20_asset", "v3") + logic("b20_stablecoin", "v3"),
    rubric=[r("edit_v3_not_v4", "The plan edits the existing V3 rather than creating V4, because Denim is not scheduled.", critical=True),
            r("order", "The plan orders executor, then sender, then receiver.")],
    notes="V3 already exists and Denim is unscheduled. Denim previously ran the V3 copy of V2: transfers by a blocked executor that succeeded now revert, and executor==sender transfers skip a policy read.")

case("denim-inverted-policy-ids", ref="eda899b42",
    category="scaffold_logic_abi", polarity="positive", split="dev",
    ticket="Add inverted policy IDs at Denim: a high bit on a policy ID inverts its authorization result, plus an invertedPolicyId(uint64) view selector. Beryl and Cobalt must keep rejecting the new selector exactly as they do today.",
    approach="edit_in_place", activation_fork="Denim", versions_=versions(modify=["V3"], frozen=["V1", "V2"]),
    must_not_modify=logic("policy", "v1", "v2"), symmetric=["policy"], surf=["abi", "revert_bytes"],
    tests_new=["V1 and V2 reject the new selector", "V3 toggles the bit"],
    critical=logic("policy", "v3") + [f"{S}/policy/versions.rs"],
    extra_allowed=[f"{S}/policy/**"],
    rubric=[r("new_abi_surface", "The plan adds a new frozen ABI surface for Denim rather than editing the Cobalt ABI.", critical=True),
            r("fail_closed", "The plan makes an inverted ID for a nonexistent policy fail closed.")],
    notes="Logic plus ABI in unshipped V3.")

case("denim-reject-token-as-credit-recipient", ref="72fb45f84",
    category="revert_bytes", polarity="positive", split="holdout",
    ticket="Transfers, mints and seizes can credit the token's own address, stranding the balance. From Denim, reject the token itself as a credit recipient with InvalidReceiver, for asset and stablecoin. Other B-20 token addresses stay valid recipients.",
    approach="edit_in_place", activation_fork="Denim", versions_=versions(modify=["V3"], frozen=["V1", "V2"]),
    must_not_modify=FROZEN_12, symmetric=["b20_asset", "b20_stablecoin"], surf=["revert_bytes"],
    tests_new=["transfer, mint and seize to the token revert InvalidReceiver"],
    critical=logic("b20_asset", "v3") + logic("b20_stablecoin", "v3"),
    rubric=[r("cobalt_frozen", "The plan leaves V2 unchanged because Cobalt is scheduled.", critical=True),
            r("all_credit_paths", "The plan covers transfer, transferFrom, mint and seize.")],
    notes="Cobalt is scheduled on mainnet and Sepolia at this commit.")

# =============================================================================================
# Era 5: Cobalt live at the latest commit. Synthetic tasks.
# =============================================================================================

case("head-reject-backport-credit-recipient", base=SYNTHETIC_BASE,
    category="old_fork_change", polarity="negative", split="dev",
    ticket="Tokens on mainnet can still be sent to their own address and the balance is stuck, because the Denim fix applies only from Denim. Apply the same token-as-recipient rejection to the existing V1 and V2 asset and stablecoin logic so mainnet users are protected now.",
    verdict="reject", approach="none", activation_fork=None, versions_=versions(frozen=["V1", "V2"]),
    must_not_modify=FROZEN_12, create=[], modify=[], critical=[],
    rubric=[r("no_partial_edit", "The plan does not propose editing V1 or V2, even partially or behind a flag.", critical=True),
            r("explains_replay", "The plan explains that changing V1 or V2 changes how already-produced blocks execute.", critical=True), REJECT_RUBRIC],
    notes="Beryl and Cobalt are live on mainnet. The only safe answer is to decline.")

case("head-reject-meter-v1-permit", base=SYNTHETIC_BASE,
    category="gas_semantics", polarity="negative", split="dev",
    ticket="Permit in the V1 asset and stablecoin logic still does signature recovery without charging gas. Add the same metering V2 has to V1 permit so the DoS vector is closed on every version.",
    verdict="reject", approach="none", activation_fork=None, versions_=versions(frozen=["V1", "V2"]),
    must_not_modify=FROZEN_12, create=[], modify=[], critical=[],
    rubric=[r("gas_frozen", "The plan states V1's gas schedule cannot change because Beryl blocks must replay with the same gas.", critical=True), REJECT_RUBRIC],
    notes="V1 only executes historical blocks once Cobalt is live.")

case("head-reject-edit-live-v2-seize", base=SYNTHETIC_BASE,
    category="old_fork_change", polarity="negative", split="holdout",
    ticket="seizeWithMemo in the V2 asset and stablecoin logic lets a seizer send seized funds to the token's own address. Patch V2 seize to reject that recipient.",
    verdict="reject", approach="none", activation_fork=None, versions_=versions(frozen=["V1", "V2"]),
    must_not_modify=FROZEN_12, create=[], modify=[], critical=[],
    rubric=[r("cobalt_live", "The plan recognizes Cobalt is live, so V2 cannot change.", critical=True), REJECT_RUBRIC],
    notes="Contrast with August cases where V2 was unshipped and editable.")

case("head-reject-delete-v1", base=SYNTHETIC_BASE,
    category="deprecate_old", polarity="negative", split="dev",
    ticket="Cobalt is live on mainnet and Sepolia, so AssetV1 and StablecoinV1 look like dead code. Delete them and their golden suites to reduce maintenance.",
    verdict="reject", approach="none", activation_fork=None, versions_=versions(frozen=["V1", "V2"]),
    must_not_modify=FROZEN_12, create=[], modify=[], critical=[],
    rubric=[r("replay_needs_v1", "The plan explains that syncing from genesis and proving historical blocks still execute V1.", critical=True), REJECT_RUBRIC],
    notes="Removing a shipped version breaks historical sync and fault proofs.")

case("head-test-only-v3-golden", base=SYNTHETIC_BASE,
    category="old_fork_change", polarity="negative", split="holdout",
    ticket="Add golden test coverage in the policy V3 golden suite pinning that isAuthorized returns true for uncreated BLOCKLIST and INTERSECT policy IDs while policyExists returns false. Do not change any behavior.",
    approach="edit_in_place", activation_fork=None, versions_=versions(frozen=["V1", "V2", "V3"]),
    must_not_modify=logic("policy", "v1", "v2", "v3"), symmetric=[],
    create=[], modify=[f"{T}/b20_policy_v3_golden.rs"], critical=[f"{T}/b20_policy_v3_golden.rs"],
    tests_new=["V3 golden matrix for uncreated IDs"],
    rubric=[r("test_only", "The plan changes only tests and no logic.", critical=True)],
    notes="Modelled on 56bcbfb24.")

case("head-everest-reject-zero-amount-burn-blocked", base=SYNTHETIC_BASE,
    category="scaffold_logic", polarity="positive", split="dev",
    ticket="burnBlocked on asset and stablecoin tokens accepts a zero amount and emits a BurnedBlocked event with nothing burned. From the Everest upgrade, revert InvalidAmount when amount is zero. Denim must keep its current behavior because the Denim release is being finalized.",
    approach="new_logic_version", activation_fork="Everest", versions_=versions(create=["V4"], frozen=["V1", "V2", "V3"]),
    must_not_modify=FROZEN_12 + logic("b20_asset", "v3") + logic("b20_stablecoin", "v3") + golden("asset", "v3") + golden("stablecoin", "v3"),
    symmetric=["b20_asset", "b20_stablecoin"], surf=["revert_bytes"],
    create=logic("b20_asset", "v4") + logic("b20_stablecoin", "v4") + golden("asset", "v4") + golden("stablecoin", "v4"),
    modify=[f"{S}/b20_asset/versions.rs", f"{S}/b20_stablecoin/versions.rs", f"{S}/b20_asset/logic/mod.rs", f"{S}/b20_stablecoin/logic/mod.rs", f"{S}/lib.rs", f"{P}/Cargo.toml"],
    critical=logic("b20_asset", "v4") + logic("b20_stablecoin", "v4") + [f"{S}/b20_asset/versions.rs", f"{S}/b20_stablecoin/versions.rs"],
    extra_allowed=[f"{S}/b20_asset/**", f"{S}/b20_stablecoin/**"],
    tests_new=["Everest resolves to V4 and Denim to V3", "V4 burnBlocked(0) reverts InvalidAmount", "V3 burnBlocked(0) still succeeds"],
    rubric=[r("v3_frozen_reason", "The plan leaves V3 unchanged because the ticket freezes Denim behavior."),
            r("v4_copy_then_change", "The plan creates V4 from V3 and applies the zero-amount check only in V4.", critical=True)],
    notes="Positive new-version case at the latest commit. Everest exists in BaseUpgrade but has no precompile version yet.")


def build(defn):
    is_historical = defn["ref"] is not None
    if is_historical:
        ref = full_sha(defn["ref"])
        base = full_sha(defn["ref"] + "^")
        added, changed = diff_name_status(base, ref)
        create, modify = sorted(added), sorted(changed)
    else:
        ref, base = None, defn["base"]
        create, modify = defn["create"], defn["modify"]
    expected = {
        "verdict": defn["verdict"],
        "approach": defn["approach"],
        "activation_fork": defn["activation_fork"],
        "versions": defn["versions_"],
        "files": {"create": create, "modify": modify, "must_not_modify": sorted(set(defn["must_not_modify"]))},
        "symmetric_modules": list(defn["symmetric"]),
        "surfaces": surfaces(*defn["surf"]),
        "tests": {"existing_goldens_unchanged": defn["goldens_unchanged"], "new": list(defn["tests_new"])},
    }
    graded = defn["graded_surfaces"]
    if graded is None:
        graded = ALL_SURFACES
    return {
        "id": defn["case_id"],
        "category": defn["category"],
        "polarity": defn["polarity"],
        "split": defn["split"],
        "source": "historical" if is_historical else "synthetic",
        "base_commit": base,
        "reference_commit": ref,
        "ticket": defn["ticket"],
        "fork_state": fork_state(base),
        "expected": expected,
        "grading": {
            "critical_files": list(defn["critical"]),
            "allowed_extra_files": sorted(set(DEFAULT_ALLOWED) | set(defn["extra_allowed"])),
            "alternatives": list(defn["alternatives"]),
            "graded_surfaces": list(graded),
        },
        "rubric": defn["rubric"],
        "notes": defn["notes"],
    }


def main():
    CASES.mkdir(exist_ok=True)
    for old in CASES.glob("*.json"):
        old.unlink()
    for defn in CASES_DEF:
        (CASES / f"{defn['case_id']}.json").write_text(json.dumps(build(defn), indent=2) + "\n")
    print(f"wrote {len(CASES_DEF)} cases")


if __name__ == "__main__":
    main()
