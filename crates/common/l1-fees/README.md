# base-common-l1-fees

Engine-neutral OP-stack L1 fee schedule.

Holds the L1 fee parameters ([`L1FeeParams`]) and the pure L1 data-cost and
operator-fee math (Bedrock / Ecotone / Fjord, plus the Isthmus operator fee),
parameterized by [`base_common_genesis::BaseUpgrade`]. It has no execution-engine
dependency, so it is shared by both `base-common-evm` (revm) and
`base-common-evm2` (EVM2), each of which adapts it with its own engine-specific
state loading and caching.

Fork gating mirrors `BaseSpecId::is_enabled_in` (upgrade-discriminant ordering).
Calldata compression estimation is delegated to `base-common-flz`.
