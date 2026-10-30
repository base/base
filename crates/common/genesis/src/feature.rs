use crate::BaseUpgrade;

macro_rules! features {
    ($($upgrade:ident { $($(#[$doc:meta])* $feature:ident,)+ },)+) => {
        /// Independently meaningful Base protocol behaviors and their activation upgrades.
        ///
        /// This registry describes *what* is enabled, not how a particular schedule resolves
        /// activation. Rollup, chain and EVM adapters retain their historical cascade,
        /// timestamp and ordinal semantics respectively.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum BaseFeature {
            $($($(#[$doc])* $feature,)+)+
        }

        impl BaseFeature {
            /// Every registered behavior, in registry order.
            pub const ALL: &'static [Self] = &[$($(Self::$feature,)+)+];

            /// The network upgrade that introduces this behavior.
            pub const fn activation_upgrade(self) -> BaseUpgrade {
                match self { $($(Self::$feature => BaseUpgrade::$upgrade,)+)+ }
            }
        }
    };
}

features! {
    Bedrock {
        /// Require child block timestamps to advance, subject to the subsecond schedule exception.
        MonotonicBlockTimestamps,
    },
    Regolith {
        /// Enforce deposit gas limits and reject the legacy system-transaction flag.
        DepositGasAccounting,
        /// Include the sender's pre-execution nonce in deposit receipts.
        DepositNonce,
        /// Do not add the legacy signature-byte allowance to L1 data gas.
        ActualL1SignatureBytes,
    },
    Canyon {
        /// Include the version field in deposit receipts.
        DepositReceiptVersion,
        /// Install the deterministic CREATE2 deployer at the transition.
        Create2Deployer,
        /// Include empty withdrawals in block bodies and payload attributes.
        Withdrawals,
        /// Use the revised EIP-1559 base fee change denominator.
        BaseFeeDenominator,
        /// Read the first ready channel rather than blocking on the oldest channel.
        ReadyChannelSelection,
        /// Publish and accept withdrawal-capable gossip payloads.
        GossipPayloadV2,
    },
    Delta {
        /// Permit span batches in derivation.
        SpanBatches,
    },
    Ecotone {
        /// Price L1 data with separate calldata and blob fee scalars, encode compact blob-aware L1
        /// attributes after the transition block, and validate blob-aware system config scalars
        /// while ignoring legacy overhead.
        BlobAwareL1Fees,
        /// Include blob gas fields in L2 headers, initially with zero values.
        BlobHeader,
        /// Include the parent beacon block root and apply the beacon-roots system call.
        BeaconRoots,
        /// Set the L2 EVM blob base fee to the protocol's constant minimum.
        ConstantBlobBaseFee,
        /// Read blob batch data after the explicitly configured L1 timestamp boundary.
        BlobDataSource,
        /// Use the blob-capable V3 Engine API payload methods.
        EngineApiV3,
        /// Inject the Ecotone contract-upgrade deposit bundle at the transition.
        EcotoneUpgradeTransactions,
        /// Publish and accept blob-capable gossip payloads.
        GossipPayloadV3,
    },
    Fjord {
        /// Estimate compressed transaction size with `FastLZ` for L1 data fees.
        FastLzL1Fees,
        /// Verify secp256r1 signatures using the original RIP-7212 gas price.
        Rip7212Precompile,
        /// Use the fixed 1800-second sequencer drift bound.
        FixedSequencerDrift,
        /// Accept the larger decompressed channel byte limit.
        LargeChannels,
        /// Accept and produce Brotli-compressed batch channels.
        BrotliCompression,
        /// Expand the buffered channel bank's maximum size.
        ChannelBankSizeExpansion,
        /// Inject the Fjord contract-upgrade deposit bundle at the transition.
        FjordUpgradeTransactions,
    },
    Granite {
        /// Bound bn254 pairing input size before executing the precompile.
        Bn254PairingInputLimit,
        /// Use the shortened channel timeout.
        ShortChannelTimeout,
        /// Exclude transaction-pool transactions from the Granite transition block.
        GraniteTransitionNoTxPool,
    },
    Holocene {
        /// Read dynamic EIP-1559 parameters from the block header extra data.
        DynamicBaseFeeParameters,
        /// Apply strict batch ordering and revised invalid-batch handling, assemble one channel at
        /// a time, drop out-of-order frames, and reset the pipeline when crossing into these rules.
        StrictDerivationOrdering,
        /// Retry invalid execution payloads as deposits-only payloads.
        DepositsOnlyOnInvalidPayload,
        /// Exclude transaction-pool transactions from the Holocene transition block.
        HoloceneTransitionNoTxPool,
    },
    PectraBlobSchedule {
        /// Apply the optional L1 Pectra blob schedule correction.
        BlobScheduleCorrection,
    },
    Isthmus {
        /// Charge the operator fee in addition to execution and L1 data fees, and include its fields
        /// in the L1 attributes deposit after the transition block.
        OperatorFee,
        /// Bound BLS12-381 multi-scalar multiplication and pairing input sizes.
        BlsInputLimits,
        /// Permit EIP-7702 set-code transactions in batches.
        Eip7702,
        /// Apply the historical-block-hash storage system call.
        HistoryStorage,
        /// Put the L2-to-L1 message-passer storage root in the header withdrawals root.
        L2ToL1MessageRoot,
        /// Set the header requests hash to the empty execution-requests hash.
        EmptyRequestsHash,
        /// Use execution-request-capable V4 Engine API payload methods.
        EngineApiV4,
        /// Inject the Isthmus contract-upgrade deposit bundle at the transition.
        IsthmusUpgradeTransactions,
        /// Publish and accept execution-request-capable gossip payloads.
        GossipPayloadV4,
        /// Reject proof derivation that exhausts its source before the claimed block.
        StrictProofSourceExhaustion,
    },
    Jovian {
        /// Scale the operator fee scalar component by 100 instead of dividing by one million.
        ScaledOperatorFees,
        /// Meter transaction DA footprint against the block budget, and include the DA footprint
        /// scalar in L1 attributes after the transition block.
        DaFootprint,
        /// Calculate the base fee from the larger of execution gas and DA footprint, with a floor.
        DaAwareBaseFee,
        /// Tighten the bn254 pairing input limit.
        TightBn254PairingInputLimit,
        /// Tighten BLS12-381 multi-scalar multiplication and pairing input limits.
        TightBlsInputLimits,
        /// Enforce the minimum base fee carried in block extra data.
        MinBaseFee,
        /// Inject the Jovian contract-upgrade deposit bundle at the transition.
        JovianUpgradeTransactions,
    },
    Azul {
        /// Use the Osaka price for the secp256r1 signature verifier.
        RepricedP256Precompile,
        /// Configure the Osaka per-transaction gas limit cap in Base EVM environments.
        TransactionGasLimitCap,
        /// Omit receipt and balance metadata from published flashblocks.
        CompactFlashblockMetadata,
        /// Request V5 payload envelopes from the execution engine.
        EngineApiV5,
    },
    Beryl {
        /// Resolve dynamic B20 token addresses to their native precompile implementations.
        TokenPrecompiles,
        /// Install the native activation registry for token and policy precompiles.
        ActivationRegistry,
        /// Enable the first B20 asset contract implementation.
        AssetPrecompile,
        /// Enable the first B20 stablecoin contract implementation.
        StablecoinPrecompile,
        /// Enable the first B20 policy contract implementation.
        PolicyPrecompile,
        /// Enable the B20 factory contract implementation.
        TokenFactoryPrecompile,
    },
    Cobalt {
        /// Use the second B20 asset contract implementation.
        AssetPrecompileV2,
        /// Use the second B20 stablecoin contract implementation.
        StablecoinPrecompileV2,
        /// Use the second B20 policy contract implementation.
        PolicyPrecompileV2,
        /// Clear retired native-precompile dynamic storage slots and charge the preparatory reads.
        DynamicStorageTailCleanup,
        /// Return only the selector, without decoder diagnostics, for native-precompile ABI errors.
        CompactAbiDecodeErrors,
        /// Reject nonzero call value in activation registry calls.
        ActivationRegistryRejectsValue,
        /// Read the activation administrator from storage, falling back to chain configuration.
        StatefulActivationAdmin,
        /// Install the transaction-context precompile backed by transient storage.
        TransactionContextPrecompile,
        /// Install the native two-dimensional nonce manager precompile.
        NonceManagerPrecompile,
    },
    Denim {
        /// Use the third B20 asset contract implementation.
        AssetPrecompileV3,
        /// Use the third B20 stablecoin contract implementation.
        StablecoinPrecompileV3,
        /// Use the third B20 policy contract implementation.
        PolicyPrecompileV3,
        /// Use deterministic 200ms block timestamps and the Base time predeploy, scale legacy
        /// whole-second gas parameters, and retain the parent's L1 origin within the same second.
        SubsecondBlocks,
        /// Use the extended six-hour sequencer drift bound.
        ExtendedSequencerDrift,
        /// Build and finalize payloads once instead of replacing them for higher fees.
        OneShotPayloadBuilding,
        /// Reject span batches and encode only singular batches for subsecond blocks.
        SingularBatchesOnly,
    },
    Everest {
        /// Execute enshrined EIP-8130 account-abstraction transactions.
        Eip8130,
        /// Expose EIP-8430 payer resolution RPC methods.
        Eip8430,
        /// Plant a nonempty code stub on the native nonce manager to preserve its persistent state.
        NativeNonceManager,
    },
    Zenith {
        /// Enable experimental EIP-8130 keystore support on genesis-configured chains only.
        Keystore,
        /// Bill transactions for their full gas limit rather than only the gas they use.
        GasLimitBilling,
    },
}

impl BaseFeature {
    /// Evaluate related features with one activation read per distinct upgrade.
    ///
    /// `is_active` must use the same schedule, timestamp and activation semantics for all
    /// features. Behaviors sharing an upgrade then share its first observed activation state,
    /// even if the runtime schedule changes during evaluation. Distinct upgrades are read
    /// separately in feature order; this is not a whole-schedule snapshot.
    ///
    /// Use this only where behavior previously shared one gate, not to combine historically
    /// separate schedule reads.
    pub fn activation_states<const N: usize>(
        features: [Self; N],
        mut is_active: impl FnMut(Self) -> bool,
    ) -> [bool; N] {
        let mut active = [false; N];
        for (index, feature) in features.iter().enumerate() {
            active[index] = features[..index]
                .iter()
                .position(|other| other.activation_upgrade() == feature.activation_upgrade())
                .map_or_else(|| is_active(*feature), |previous| active[previous]);
        }
        active
    }
}

impl BaseUpgrade {
    /// Whether this execution upgrade includes `feature`, using historical ordinal semantics.
    ///
    /// Timestamp-based consumers must use their rollup or chain schedule adapter instead.
    pub const fn is_feature_enabled(self, feature: BaseFeature) -> bool {
        feature.activation_upgrade() as u8 <= self as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn related_features_share_activation_state_but_distinct_upgrades_do_not() {
        for initial in [false, true] {
            let mut live_state = initial;
            let states = BaseFeature::activation_states(
                [
                    BaseFeature::DepositGasAccounting,
                    BaseFeature::Withdrawals,
                    BaseFeature::DepositNonce,
                ],
                |_| {
                    let observed = live_state;
                    live_state = !initial;
                    observed
                },
            );
            assert_eq!(states, [initial, !initial, initial]);
        }
    }
}
