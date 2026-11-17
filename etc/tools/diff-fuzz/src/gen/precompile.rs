//! Inputs for the Ethereum precompiles and Base's P256 verifier, valid and invalid.

use alloy_primitives::{Address, B256, U256, Uint, address, hex, keccak256};
use alloy_signer::SignerSync;
use base_common_genesis::{BaseFeature, BaseUpgrade};
use base_common_precompiles::{
    GRANITE_MAX_INPUT_SIZE, ISTHMUS_G1_MSM_MAX_INPUT_SIZE, ISTHMUS_G2_MSM_MAX_INPUT_SIZE,
    ISTHMUS_PAIRING_MAX_INPUT_SIZE, JOVIAN_G1_MSM_MAX_INPUT_SIZE, JOVIAN_G2_MSM_MAX_INPUT_SIZE,
    JOVIAN_MAX_INPUT_SIZE, JOVIAN_PAIRING_MAX_INPUT_SIZE,
};
use p256::ecdsa::{SigningKey as P256Key, signature::hazmat::PrehashSigner};
use sha2::{Digest, Sha256};

use crate::r#gen::{keys::Keys, rng::Rng};

/// A call to a precompile: `prefix || unit * repeat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrecompileCall {
    /// Precompile address.
    pub address: Address,
    /// Leading bytes of the input.
    pub prefix: Vec<u8>,
    /// Repeated unit following the prefix.
    pub unit: Vec<u8>,
    /// Number of unit repetitions.
    pub repeat: usize,
    /// The exact gas the call costs when it is computable without executing it.
    pub cost: Option<u64>,
}

impl PrecompileCall {
    const fn plain(address: Address, input: Vec<u8>, cost: Option<u64>) -> Self {
        Self { address, prefix: input, unit: Vec::new(), repeat: 0, cost }
    }

    /// The full input.
    pub fn input(&self) -> Vec<u8> {
        let mut input = self.prefix.clone();
        for _ in 0..self.repeat {
            input.extend_from_slice(&self.unit);
        }
        input
    }

    /// Total input length.
    pub const fn len(&self) -> usize {
        self.prefix.len() + self.unit.len() * self.repeat
    }

    /// Whether the input is empty.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Precompile input generator.
#[derive(Debug)]
pub struct Precompiles;

/// BN254 base field modulus.
const BN_P: U256 =
    Uint::from_be_bytes(hex!("30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd47"));
/// BN254 G2 generator, encoded as `x_imag || x_real || y_imag || y_real`.
const BN_G2: [u8; 128] = hex!(
    "198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2"
    "1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed"
    "090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b"
    "12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa"
);
/// BLS12-381 base field modulus.
const BLS_P: [u8; 48] = hex!(
    "1a0111ea397fe69a4b1ba7b6434bacd764774b84f38512bf6730d2a0f6b0f6241eabfffeb153ffffb9feffffffffaaab"
);
/// BLS12-381 G1 generator coordinates.
const BLS_G1_X: [u8; 48] = hex!(
    "17f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb"
);
const BLS_G1_Y: [u8; 48] = hex!(
    "08b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e1"
);
/// BLS12-381 G2 generator coordinates (`c0`, `c1` per coordinate).
const BLS_G2: [[u8; 48]; 4] = [
    hex!(
        "024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb8"
    ),
    hex!(
        "13e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e"
    ),
    hex!(
        "0ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801"
    ),
    hex!(
        "0606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79be"
    ),
];

impl Precompiles {
    /// `ecrecover`.
    pub const ECRECOVER: Address = address!("0x0000000000000000000000000000000000000001");
    /// `sha256`.
    pub const SHA256: Address = address!("0x0000000000000000000000000000000000000002");
    /// `ripemd160`.
    pub const RIPEMD160: Address = address!("0x0000000000000000000000000000000000000003");
    /// `identity`.
    pub const IDENTITY: Address = address!("0x0000000000000000000000000000000000000004");
    /// `modexp`.
    pub const MODEXP: Address = address!("0x0000000000000000000000000000000000000005");
    /// BN254 addition.
    pub const BN_ADD: Address = address!("0x0000000000000000000000000000000000000006");
    /// BN254 scalar multiplication.
    pub const BN_MUL: Address = address!("0x0000000000000000000000000000000000000007");
    /// BN254 pairing check.
    pub const BN_PAIRING: Address = address!("0x0000000000000000000000000000000000000008");
    /// BLAKE2 compression.
    pub const BLAKE2F: Address = address!("0x0000000000000000000000000000000000000009");
    /// KZG point evaluation (Ecotone).
    pub const POINT_EVALUATION: Address = address!("0x000000000000000000000000000000000000000a");
    /// BLS12-381 G1 addition (Isthmus).
    pub const BLS_G1_ADD: Address = address!("0x000000000000000000000000000000000000000b");
    /// BLS12-381 G1 MSM.
    pub const BLS_G1_MSM: Address = address!("0x000000000000000000000000000000000000000c");
    /// BLS12-381 G2 addition.
    pub const BLS_G2_ADD: Address = address!("0x000000000000000000000000000000000000000d");
    /// BLS12-381 G2 MSM.
    pub const BLS_G2_MSM: Address = address!("0x000000000000000000000000000000000000000e");
    /// BLS12-381 pairing check.
    pub const BLS_PAIRING: Address = address!("0x000000000000000000000000000000000000000f");
    /// BLS12-381 map field element to G1.
    pub const BLS_MAP_G1: Address = address!("0x0000000000000000000000000000000000000010");
    /// BLS12-381 map field element to G2.
    pub const BLS_MAP_G2: Address = address!("0x0000000000000000000000000000000000000011");
    /// secp256r1 signature verification (Fjord).
    pub const P256VERIFY: Address = address!("0x0000000000000000000000000000000000000100");

    /// Precompile addresses active under `upgrade`.
    pub fn active(upgrade: BaseUpgrade) -> Vec<Address> {
        let mut out = vec![
            Self::ECRECOVER,
            Self::SHA256,
            Self::RIPEMD160,
            Self::IDENTITY,
            Self::MODEXP,
            Self::BN_ADD,
            Self::BN_MUL,
            Self::BN_PAIRING,
            Self::BLAKE2F,
        ];
        if upgrade.is_feature_enabled(BaseFeature::BeaconRoots) {
            out.push(Self::POINT_EVALUATION);
        }
        if upgrade.is_feature_enabled(BaseFeature::Rip7212Precompile) {
            out.push(Self::P256VERIFY);
        }
        if upgrade.is_feature_enabled(BaseFeature::BlsInputLimits) {
            out.extend([
                Self::BLS_G1_ADD,
                Self::BLS_G1_MSM,
                Self::BLS_G2_ADD,
                Self::BLS_G2_MSM,
                Self::BLS_PAIRING,
                Self::BLS_MAP_G1,
                Self::BLS_MAP_G2,
            ]);
        }
        out
    }

    /// Generates an input for a random active precompile (or, rarely, an inactive one).
    pub fn random(rng: &mut Rng, upgrade: BaseUpgrade) -> PrecompileCall {
        let active = Self::active(upgrade);
        let address = if rng.chance(3) {
            // A precompile from a later fork: a plain account in this era.
            *rng.pick(&[
                Self::POINT_EVALUATION,
                Self::P256VERIFY,
                Self::BLS_PAIRING,
                Self::BLS_MAP_G1,
            ])
        } else {
            *rng.pick(&active)
        };
        Self::for_address(rng, upgrade, address)
    }

    /// Generates an input for `address`.
    pub fn for_address(rng: &mut Rng, upgrade: BaseUpgrade, address: Address) -> PrecompileCall {
        let valid = rng.chance(70);
        let mut call = match address {
            a if a == Self::ECRECOVER => Self::ecrecover(rng, valid),
            a if a == Self::SHA256 || a == Self::RIPEMD160 || a == Self::IDENTITY => {
                let len = Self::data_len(rng);
                let words = (len as u64).div_ceil(32);
                let cost = match a {
                    a if a == Self::SHA256 => 60 + 12 * words,
                    a if a == Self::RIPEMD160 => 600 + 120 * words,
                    _ => 15 + 3 * words,
                };
                PrecompileCall::plain(a, rng.bytes(len), Some(cost))
            }
            a if a == Self::MODEXP => Self::modexp(rng),
            a if a == Self::BN_ADD => Self::bn_add(rng, valid),
            a if a == Self::BN_MUL => Self::bn_mul(rng, valid),
            a if a == Self::BN_PAIRING => Self::bn_pairing(rng, upgrade, valid),
            a if a == Self::BLAKE2F => Self::blake2f(rng, valid),
            a if a == Self::POINT_EVALUATION => Self::point_evaluation(rng, valid),
            a if a == Self::P256VERIFY => Self::p256(rng, upgrade, valid),
            a if a == Self::BLS_G1_ADD => {
                let mut input = Self::bls_g1(false);
                input.extend(Self::bls_g1(rng.chance(50)));
                Self::corrupt(rng, valid, &mut input);
                PrecompileCall::plain(a, input, Some(375))
            }
            a if a == Self::BLS_G2_ADD => {
                let mut input = Self::bls_g2();
                input.extend(Self::bls_g2());
                Self::corrupt(rng, valid, &mut input);
                PrecompileCall::plain(a, input, Some(600))
            }
            a if a == Self::BLS_G1_MSM || a == Self::BLS_G2_MSM || a == Self::BLS_PAIRING => {
                Self::bls_multi(rng, upgrade, a, valid)
            }
            a if a == Self::BLS_MAP_G1 => {
                let input = Self::bls_fp(rng, valid);
                PrecompileCall::plain(a, input, Some(5_500))
            }
            a if a == Self::BLS_MAP_G2 => {
                let mut input = Self::bls_fp(rng, true);
                input.extend(Self::bls_fp(rng, valid));
                PrecompileCall::plain(a, input, Some(23_800))
            }
            a => PrecompileCall::plain(
                a,
                {
                    let len = Self::data_len(rng);
                    rng.bytes(len)
                },
                None,
            ),
        };
        if rng.chance(5) {
            // Truncate or extend the input by a little.
            if !call.prefix.is_empty() && rng.chance(50) {
                let cut = rng.index(call.prefix.len());
                call.prefix.truncate(cut);
            } else {
                call.prefix.extend(rng.bytes_between(1, 40));
            }
            call.cost = None;
        }
        call
    }

    const fn data_len(rng: &mut Rng) -> usize {
        match rng.below(5) {
            0 => 0,
            1 => rng.range(1, 31) as usize,
            2 => 32 * rng.range(1, 8) as usize,
            3 => rng.range(1, 1_000) as usize,
            _ => rng.range(1_000, 8_000) as usize,
        }
    }

    fn corrupt(rng: &mut Rng, valid: bool, input: &mut [u8]) {
        if !valid && !input.is_empty() {
            let at = rng.index(input.len());
            input[at] ^= 1 << rng.below(8);
        }
    }

    const fn word(value: U256) -> [u8; 32] {
        value.to_be_bytes()
    }

    fn ecrecover(rng: &mut Rng, valid: bool) -> PrecompileCall {
        let keys = Keys::get();
        let hash = rng.b256();
        let signer = &keys.signers[rng.index(Keys::COUNT)];
        let signature = signer.sign_hash_sync(&hash).expect("signing never fails");
        let mut v = U256::from(27 + u64::from(signature.v()));
        let mut r = signature.r();
        let mut s = signature.s();
        if !valid {
            match rng.below(5) {
                0 => v = U256::from(rng.below(64)),
                1 => r = U256::ZERO,
                2 => s = U256::MAX,
                3 => v += U256::from(1) << 200,
                _ => r = rng.u256(),
            }
        }
        let mut input = hash.to_vec();
        input.extend(Self::word(v));
        input.extend(Self::word(r));
        input.extend(Self::word(s));
        PrecompileCall::plain(Self::ECRECOVER, input, Some(3_000))
    }

    fn modexp(rng: &mut Rng) -> PrecompileCall {
        let pick_len = |rng: &mut Rng| -> usize {
            match rng.below(8) {
                0 => 0,
                1 => 1,
                2 => 32,
                3 => 64,
                4 => 1_024,
                5 => 1_025,
                6 => rng.range(1, 200) as usize,
                _ => rng.range(1, 600) as usize,
            }
        };
        let (base_len, exp_len, mod_len) = (pick_len(rng), pick_len(rng).min(64), pick_len(rng));
        let mut input = Vec::new();
        input.extend(Self::word(U256::from(base_len)));
        input.extend(Self::word(U256::from(exp_len)));
        input.extend(Self::word(U256::from(mod_len)));
        if rng.chance(3) {
            // Declared lengths far beyond the data: priced from the header alone.
            input[24..32].copy_from_slice(&u64::MAX.to_be_bytes());
            return PrecompileCall::plain(Self::MODEXP, input, None);
        }
        input.extend(rng.bytes(base_len));
        let exp: Vec<u8> = match rng.below(4) {
            0 => vec![0; exp_len],
            1 => vec![0xff; exp_len],
            _ => rng.bytes(exp_len),
        };
        input.extend(exp);
        let mut modulus = rng.bytes(mod_len);
        if rng.chance(20) {
            modulus.iter_mut().for_each(|b| *b = 0);
        }
        input.extend(modulus);
        PrecompileCall::plain(Self::MODEXP, input, None)
    }

    fn bn_g1(rng: &mut Rng, valid: bool) -> [u8; 64] {
        let mut out = [0u8; 64];
        match (valid, rng.below(3)) {
            (true, 0) => {}
            (true, 1) => {
                out[..32].copy_from_slice(&Self::word(U256::from(1)));
                out[32..].copy_from_slice(&Self::word(U256::from(2)));
            }
            (true, _) => {
                out[..32].copy_from_slice(&Self::word(U256::from(1)));
                out[32..].copy_from_slice(&Self::word(BN_P - U256::from(2)));
            }
            (false, 0) => {
                out[..32].copy_from_slice(&Self::word(U256::from(1)));
                out[32..].copy_from_slice(&Self::word(U256::from(3)));
            }
            (false, 1) => out[..32].copy_from_slice(&Self::word(BN_P)),
            (false, _) => out.copy_from_slice(&rng.bytes(64)),
        }
        out
    }

    fn bn_add(rng: &mut Rng, valid: bool) -> PrecompileCall {
        let mut input = Self::bn_g1(rng, true).to_vec();
        input.extend(Self::bn_g1(rng, valid));
        PrecompileCall::plain(Self::BN_ADD, input, Some(150))
    }

    fn bn_mul(rng: &mut Rng, valid: bool) -> PrecompileCall {
        let mut input = Self::bn_g1(rng, valid).to_vec();
        input.extend(Self::word(rng.edge_u256()));
        PrecompileCall::plain(Self::BN_MUL, input, Some(6_000))
    }

    /// `e(G1, G2) * e(-G1, G2)`, which is the identity.
    fn bn_pair_unit() -> Vec<u8> {
        let mut unit = Vec::with_capacity(384);
        unit.extend(Self::word(U256::from(1)));
        unit.extend(Self::word(U256::from(2)));
        unit.extend(BN_G2);
        unit.extend(Self::word(U256::from(1)));
        unit.extend(Self::word(BN_P - U256::from(2)));
        unit.extend(BN_G2);
        unit
    }

    fn bn_pairing(rng: &mut Rng, upgrade: BaseUpgrade, valid: bool) -> PrecompileCall {
        let max = if upgrade.is_feature_enabled(BaseFeature::TightBn254PairingInputLimit) {
            JOVIAN_MAX_INPUT_SIZE
        } else if upgrade.is_feature_enabled(BaseFeature::Bn254PairingInputLimit) {
            GRANITE_MAX_INPUT_SIZE
        } else {
            0
        };
        let unit = Self::bn_pair_unit();
        let repeat = match rng.below(6) {
            0 => 0,
            1 if max > 0 => max / unit.len(),
            2 if max > 0 => max / unit.len() + 1,
            _ => rng.range(1, 3) as usize,
        };
        let mut prefix = Vec::new();
        if !valid {
            match rng.below(3) {
                0 => prefix.extend(Self::bn_g1(rng, false)),
                1 => prefix.extend(rng.bytes_between(1, 191)),
                _ => {
                    prefix.extend(Self::bn_g1(rng, true));
                    prefix.extend(rng.bytes(128));
                }
            }
        }
        let pairs = (prefix.len() + unit.len() * repeat) / 192;
        let cost = Some(45_000 + 34_000 * pairs as u64);
        PrecompileCall { address: Self::BN_PAIRING, prefix, unit, repeat, cost }
    }

    fn blake2f(rng: &mut Rng, valid: bool) -> PrecompileCall {
        let rounds: u32 = match rng.below(6) {
            0 => 0,
            1 => 12,
            2 => rng.range(1, 64) as u32,
            3 => rng.range(1_000, 200_000) as u32,
            4 => u32::MAX,
            _ => 1,
        };
        let mut input = rounds.to_be_bytes().to_vec();
        input.extend(rng.bytes(208));
        input.push(if valid { rng.below(2) as u8 } else { 2 + rng.below(250) as u8 });
        PrecompileCall::plain(Self::BLAKE2F, input, Some(u64::from(rounds)))
    }

    fn point_evaluation(rng: &mut Rng, valid: bool) -> PrecompileCall {
        // The zero polynomial: commitment and proof are the point at infinity, so `p(z) = 0`
        // for every `z` in the field.
        let mut infinity = [0u8; 48];
        infinity[0] = 0xc0;
        let mut versioned = Sha256::digest(infinity).to_vec();
        versioned[0] = 0x01;
        let mut z = rng.b256();
        z[0] &= 0x3f;
        let mut input = versioned;
        input.extend(z.0);
        input.extend([0u8; 32]);
        input.extend(infinity);
        input.extend(infinity);
        if !valid {
            match rng.below(4) {
                0 => input[0] = 0x02,
                1 => input[64 + 31] = 1,
                2 => input[32] = 0xff,
                _ => input.truncate(191),
            }
        }
        PrecompileCall::plain(Self::POINT_EVALUATION, input, Some(50_000))
    }

    fn p256(rng: &mut Rng, upgrade: BaseUpgrade, valid: bool) -> PrecompileCall {
        let secret = keccak256(rng.bytes(32));
        let key = P256Key::from_bytes(secret.as_slice().into()).unwrap_or_else(|_| {
            P256Key::from_bytes(B256::with_last_byte(7).as_slice().into()).expect("valid key")
        });
        let hash = rng.b256();
        let signature: p256::ecdsa::Signature =
            key.sign_prehash(hash.as_slice()).expect("p256 signing never fails");
        let point = key.verifying_key().to_encoded_point(false);
        let mut input = hash.to_vec();
        input.extend(signature.to_bytes());
        input.extend(point.x().expect("uncompressed").as_slice());
        input.extend(point.y().expect("uncompressed").as_slice());
        if !valid {
            match rng.below(4) {
                0 => input[0] ^= 1,
                1 => input[40] ^= 0x80,
                2 => input[159] ^= 1,
                _ => input[32..64].fill(0),
            }
        }
        let cost = if upgrade.is_feature_enabled(BaseFeature::RepricedP256Precompile) {
            6_900
        } else {
            3_450
        };
        PrecompileCall::plain(Self::P256VERIFY, input, Some(cost))
    }

    fn pad64(fp: &[u8; 48]) -> Vec<u8> {
        let mut out = vec![0u8; 16];
        out.extend_from_slice(fp);
        out
    }

    fn bls_g1(negate: bool) -> Vec<u8> {
        let mut out = Self::pad64(&BLS_G1_X);
        let y = if negate {
            let p = Uint::<384, 6>::from_be_bytes(BLS_P);
            let y = Uint::<384, 6>::from_be_bytes(BLS_G1_Y);
            (p - y).to_be_bytes::<48>()
        } else {
            BLS_G1_Y
        };
        out.extend(Self::pad64(&y));
        out
    }

    fn bls_g2() -> Vec<u8> {
        BLS_G2.iter().flat_map(Self::pad64).collect()
    }

    fn bls_fp(rng: &mut Rng, valid: bool) -> Vec<u8> {
        let mut fp = rng.bytes(48);
        if valid {
            fp[0] &= 0x0f;
        } else {
            fp.copy_from_slice(&BLS_P);
        }
        let mut out = vec![0u8; 16];
        out.extend(fp);
        out
    }

    fn bls_multi(
        rng: &mut Rng,
        upgrade: BaseUpgrade,
        address: Address,
        valid: bool,
    ) -> PrecompileCall {
        let jovian = upgrade.is_feature_enabled(BaseFeature::TightBlsInputLimits);
        let (unit, max) = if address == Self::BLS_G1_MSM {
            let mut unit = Self::bls_g1(false);
            unit.extend(Self::word(rng.edge_u256()));
            let max =
                if jovian { JOVIAN_G1_MSM_MAX_INPUT_SIZE } else { ISTHMUS_G1_MSM_MAX_INPUT_SIZE };
            (unit, max)
        } else if address == Self::BLS_G2_MSM {
            let mut unit = Self::bls_g2();
            unit.extend(Self::word(rng.edge_u256()));
            let max =
                if jovian { JOVIAN_G2_MSM_MAX_INPUT_SIZE } else { ISTHMUS_G2_MSM_MAX_INPUT_SIZE };
            (unit, max)
        } else {
            let mut unit = Self::bls_g1(false);
            unit.extend(Self::bls_g2());
            unit.extend(Self::bls_g1(true));
            unit.extend(Self::bls_g2());
            let max =
                if jovian { JOVIAN_PAIRING_MAX_INPUT_SIZE } else { ISTHMUS_PAIRING_MAX_INPUT_SIZE };
            (unit, max)
        };
        let repeat = match rng.below(8) {
            0 => max / unit.len(),
            1 => max / unit.len() + 1,
            2 => 0,
            _ => rng.range(1, 4) as usize,
        };
        let mut prefix = Vec::new();
        if !valid {
            prefix = unit.clone();
            Self::corrupt(rng, false, &mut prefix);
        }
        PrecompileCall { address, prefix, unit, repeat, cost: None }
    }
}

#[cfg(test)]
mod tests {
    use base_common_evm::{BasePrecompiles, BaseSpecId};
    use base_common_genesis::BaseUpgrade;

    use super::{PrecompileCall, Precompiles};
    use crate::r#gen::rng::Rng;

    fn run(upgrade: BaseUpgrade, call: &PrecompileCall) -> Option<Vec<u8>> {
        let precompiles = BasePrecompiles::new_with_spec(BaseSpecId::new(upgrade));
        let precompile = precompiles.precompiles().get(&call.address)?;
        let output = precompile.execute(&call.input(), u64::MAX / 2, 0).ok()?;
        (output.halt_reason().is_none() && !output.is_revert()).then(|| output.bytes.to_vec())
    }

    #[test]
    fn valid_inputs_succeed() {
        let upgrade = BaseUpgrade::Jovian;
        let mut rng = Rng::new(&[1]);
        let one = {
            let mut word = vec![0u8; 32];
            word[31] = 1;
            word
        };
        let pairing = PrecompileCall {
            address: Precompiles::BN_PAIRING,
            prefix: Vec::new(),
            unit: Precompiles::bn_pair_unit(),
            repeat: 2,
            cost: None,
        };
        assert_eq!(run(upgrade, &pairing), Some(one.clone()));

        let bls = PrecompileCall {
            address: Precompiles::BLS_PAIRING,
            prefix: Vec::new(),
            unit: {
                let mut unit = Precompiles::bls_g1(false);
                unit.extend(Precompiles::bls_g2());
                unit.extend(Precompiles::bls_g1(true));
                unit.extend(Precompiles::bls_g2());
                unit
            },
            repeat: 1,
            cost: None,
        };
        assert_eq!(run(upgrade, &bls), Some(one.clone()));

        let mut g1_add = Precompiles::bls_g1(false);
        g1_add.extend(Precompiles::bls_g1(true));
        let sum = run(upgrade, &PrecompileCall::plain(Precompiles::BLS_G1_ADD, g1_add, None));
        assert_eq!(sum, Some(vec![0u8; 128]));

        assert!(run(upgrade, &Precompiles::point_evaluation(&mut rng, true)).is_some());
        assert!(run(upgrade, &Precompiles::point_evaluation(&mut rng, false)).is_none());
        assert_eq!(run(upgrade, &Precompiles::p256(&mut rng, upgrade, true)), Some(one));
        let ecrecover = run(upgrade, &Precompiles::ecrecover(&mut rng, true)).unwrap();
        assert_eq!(ecrecover.len(), 32);
        assert!(run(upgrade, &Precompiles::bn_add(&mut rng, true)).is_some());
    }

    #[test]
    fn pairing_limits_follow_the_fork() {
        let unit = Precompiles::bn_pair_unit();
        for (upgrade, max) in [
            (BaseUpgrade::Granite, super::GRANITE_MAX_INPUT_SIZE),
            (BaseUpgrade::Jovian, super::JOVIAN_MAX_INPUT_SIZE),
        ] {
            let fits = PrecompileCall {
                address: Precompiles::BN_PAIRING,
                prefix: Vec::new(),
                unit: unit.clone(),
                repeat: max / unit.len(),
                cost: None,
            };
            assert!(run(upgrade, &fits).is_some(), "{upgrade:?} max input");
            let over = PrecompileCall { repeat: max / unit.len() + 1, ..fits };
            assert!(run(upgrade, &over).is_none(), "{upgrade:?} over max input");
        }
    }
}
