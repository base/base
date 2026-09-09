# base-common-l1-fees

Engine-neutral OP-stack L1 fee schedule.

Holds the L1 fee parameters ([`L1FeeParams`]) and the pure L1 data-cost and
operator-fee math (Bedrock / Ecotone / Fjord, plus the Isthmus operator fee),
parameterized by [`base_common_chain_config::BaseUpgrade`]. It has no execution-engine
dependency. `base-execution-evm-runtime` supplies the state loading and caching used during
execution.

Fork gating mirrors `BaseSpecId::is_enabled_in` (upgrade-discriminant ordering).
Calldata compression estimation is delegated to `base-common-flz`.
