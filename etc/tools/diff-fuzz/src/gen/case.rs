//! Single-block cases: a pure function of `(seed, options, index)`.

use std::collections::BTreeSet;

use alloy_primitives::Address;
use base_common_precompiles::{B20TokenRole, B20Variant};

use crate::{
    Request,
    r#gen::{
        block::{L1Origin, SysConfig},
        era::{Era, Schedule},
        genesis::Genesis,
        keys::Keys,
        native::Native,
        plan::Planner,
        registry::Token,
        rng::Rng,
        scenario::Scenarios,
        template::{Template, TemplateInput, TrafficMix},
    },
};

/// Generator options shared by single-block cases and chains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratorOptions {
    /// Eras to target (at least one).
    pub eras: Vec<Era>,
    /// Minimum random transactions per block.
    pub min_txs: usize,
    /// Maximum random transactions per block.
    pub max_txs: usize,
    /// Percentage of random transactions that are deliberately invalid.
    pub invalid_percent: u64,
    /// Percentage of single-block cases placed at a fork activation block. Chains start just
    /// before a fork activation three times as often.
    pub activation_percent: u64,
    /// Whether user deposits may create contracts from the key whose nonce is `u64::MAX`.
    /// Real-node chains, which advance only on base/base's block, leave these out.
    pub max_nonce_deposit_creates: bool,
    /// Whether chains carry the state-stress contract and accounts at genesis (see
    /// [`crate::Stress`]), so blocks can be planned with a [`crate::StressPlan`].
    pub state_stress: bool,
}

impl Default for GeneratorOptions {
    fn default() -> Self {
        Self {
            eras: Era::ALL.to_vec(),
            min_txs: 4,
            max_txs: 24,
            invalid_percent: 8,
            activation_percent: 15,
            max_nonce_deposit_creates: true,
            state_stress: false,
        }
    }
}

impl GeneratorOptions {
    /// The traffic mix these options describe.
    pub const fn mix(&self) -> TrafficMix {
        TrafficMix {
            min_txs: self.min_txs,
            max_txs: self.max_txs,
            invalid_percent: self.invalid_percent,
            max_nonce_deposit_creates: self.max_nonce_deposit_creates,
        }
    }

    /// Picks an era, weighting recent eras most.
    pub fn pick_era(&self, rng: &mut Rng) -> Era {
        let weights: Vec<u64> = self
            .eras
            .iter()
            .map(|era| {
                let position = Era::ALL.iter().position(|e| e == era).unwrap_or(0) as u64;
                1 + position * position
            })
            .collect();
        self.eras[rng.weighted(&weights)]
    }
}

/// The block-number range of an era: from its activation block to the block before the next
/// era's activation (Denim: a long synthetic future).
pub fn era_blocks(era: Era) -> (u64, u64) {
    let schedule = Schedule::mainnet();
    let first =
        if era == Era::Regolith { 1 } else { schedule.first_block_at(era.activation_timestamp()) };
    let next = Era::ALL.iter().position(|e| *e == era).and_then(|i| Era::ALL.get(i + 1));
    let last = next.map_or(first + 50_000_000, |next| {
        schedule.first_block_at(next.activation_timestamp()) - 1
    });
    (first, last)
}

/// Generates independent single-block build-mode cases.
#[derive(Debug, Clone)]
pub struct Generator {
    /// Seed.
    pub seed: u64,
    /// Options.
    pub options: GeneratorOptions,
}

impl Generator {
    /// Creates a generator.
    pub fn new(seed: u64, options: GeneratorOptions) -> Self {
        assert!(!options.eras.is_empty(), "at least one era");
        Self { seed, options }
    }

    /// Case `index`.
    pub fn case(&self, index: u64) -> Request {
        self.template(index).request
    }

    /// Case `index` with one label per transaction describing what it exercises.
    pub fn template(&self, index: u64) -> Template {
        self.template_with(index, &mut |_, _| {})
    }

    /// Case `index` with `traffic` called after the case's scenario transactions and before
    /// its random traffic in each planning phase (`true` for the deposit phase, `false` for the
    /// sequencer phase), so a caller can add transactions planned against the same pre-state.
    /// With a no-op `traffic` this is [`Self::template`].
    pub fn template_with(
        &self,
        index: u64,
        traffic: &mut dyn FnMut(&mut Planner<'_>, bool),
    ) -> Template {
        let mut rng = Rng::new(&[self.seed, index]);
        let schedule = Schedule::mainnet();
        let era = self.options.pick_era(&mut rng);
        let (first, last) = era_blocks(era);
        let activation = era != Era::Regolith && rng.chance(self.options.activation_percent);
        let number = if activation { first } else { rng.range(first + 1, last) };
        let parent_number = number - 1;
        let parent_timestamp = schedule.timestamp_parts(parent_number).0;
        let state_era = Era::at(parent_timestamp);

        let sys = SysConfig::random(&mut rng, schedule.timestamp_parts(number).0);
        // BLOCKHASH can reach 256 blocks back, so requests carry the full window.
        let genesis = Genesis::build(&mut rng, state_era, parent_number, &sys, 256);
        let mut origin = L1Origin::random(&mut rng, schedule.timestamp_parts(number).0);
        if rng.chance(60) {
            origin.sequence_number = rng.range(1, 30);
        }
        let mut registry = genesis.registry.clone();
        let upgrade = schedule.upgrade_at(schedule.timestamp_parts(number).0);
        // Tokens created at the start of this block are known to the planner under their
        // derived addresses, so later transactions in the block can use them.
        let mut tokens = Vec::new();
        if Native::active(upgrade) && rng.chance(40) {
            for _ in 0..rng.range(1, 2) {
                let salt = rng.b256();
                let variant =
                    if rng.chance(50) { B20Variant::Asset } else { B20Variant::Stablecoin };
                let holders: Vec<Address> =
                    (0..3).map(|_| Keys::get().address(rng.range(1, 8) as usize)).collect();
                let address = variant.compute_address(Keys::get().operator(), salt).0;
                // `Native::seeded_token_input` gives the operator every role.
                let roles = [
                    B20TokenRole::DefaultAdmin,
                    B20TokenRole::Mint,
                    B20TokenRole::Burn,
                    B20TokenRole::BurnBlocked,
                    B20TokenRole::Seize,
                    B20TokenRole::Pause,
                    B20TokenRole::Unpause,
                    B20TokenRole::Metadata,
                ]
                .into_iter()
                .map(B20TokenRole::id)
                .chain((variant == B20Variant::Asset).then_some(Native::ASSET_OPERATOR_ROLE))
                .map(|role| (role, BTreeSet::from([Keys::get().operator()])))
                .collect();
                registry.tokens.push(Token {
                    address,
                    variant,
                    name: Native::seeded_token_name(variant).to_owned(),
                    roles,
                    holders: holders.iter().copied().collect(),
                    approvals: BTreeSet::new(),
                });
                tokens.push((salt, variant, holders));
            }
        }
        let input = TemplateInput {
            id: format!("gen-{:x}-{index}", self.seed),
            state: &genesis.state,
            registry: &registry,
            sys: &sys,
            origin: &origin,
            ancestors: &genesis.ancestors,
            time_offset: 0,
            mix: self.options.mix(),
            number_offset: 0,
        };
        let activate = Native::active(upgrade) && !Native::active(state_era.upgrade());
        Template::build(&mut rng, input, &mut |planner, deposits| {
            if !deposits && activate {
                Scenarios::activate_features(planner);
            }
            if !deposits {
                for (salt, variant, holders) in &tokens {
                    planner.token_bootstrap(*salt, *variant, holders);
                }
            }
            traffic(planner, deposits);
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Header, transaction::SignerRecoverable};
    use alloy_eips::Decodable2718;
    use base_common_consensus::BaseTxEnvelope;

    use super::{Generator, GeneratorOptions};
    use crate::{Request, r#gen::era::Era};

    #[test]
    fn cases_are_deterministic_well_formed_and_round_trip() {
        let generator = Generator::new(42, GeneratorOptions::default());
        let mut eras = std::collections::BTreeSet::new();
        for index in 0..24 {
            let template = generator.template(index);
            let request = &template.request;
            assert_eq!(request, &generator.case(index), "case {index} is not deterministic");
            assert!(request.id.contains("2a") && request.id.ends_with(&index.to_string()));
            assert_eq!(template.labels.len(), request.transactions.len());

            let json = serde_json::to_string(request).unwrap();
            let parsed: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, request);

            // Ancestors chain up and end at a parent whose state root is the witness root.
            let headers: Vec<Header> =
                request.ancestors.iter().map(|raw| alloy_rlp::decode_exact(raw).unwrap()).collect();
            for pair in headers.windows(2) {
                assert_eq!(pair[0].parent_hash, pair[1].hash_slow());
                assert_eq!(pair[0].number, pair[1].number + 1);
            }
            assert_eq!(headers[0].number + 1, request.header.number);
            let roots: Vec<_> =
                request.witness.state.iter().map(alloy_primitives::keccak256).collect();
            assert!(roots.contains(&headers[0].state_root));
            eras.insert(Era::at(request.header.timestamp));

            // Every transaction the generator meant to be decodable decodes and recovers.
            for (raw, label) in request.transactions.iter().zip(&template.labels) {
                // Blob transactions (any invalid kind may use the shape) and deliberately
                // malformed encodings do not decode as Base transactions.
                let malformed =
                    ["invalid:truncated", "invalid:unknownType", "invalid:trailingBytes"];
                if (label.starts_with("invalid:") && raw.first() == Some(&3))
                    || malformed.contains(&label.as_str())
                {
                    continue;
                }
                let tx = BaseTxEnvelope::decode_2718(&mut raw.as_ref())
                    .unwrap_or_else(|e| panic!("{label} does not decode: {e}"));
                if label != "invalid:highS" {
                    tx.recover_signer().unwrap_or_else(|e| panic!("{label} does not recover: {e}"));
                }
            }
        }
        assert!(eras.len() > 3, "cases cover several eras: {eras:?}");
        assert_ne!(generator.case(0), Generator::new(43, GeneratorOptions::default()).case(0));
    }
}
