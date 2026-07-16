//! Stateless EIP-8130 authenticator dispatch: route a signing hash + auth blob
//! to the enshrined canonical authenticator and return the resolved `actorId`.

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::{SolValue, sol};
use base_common_consensus::{Eip8130Constants, Eip8130Contracts};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::ecdsa::{
    Signature as P256Signature, VerifyingKey as P256VerifyingKey,
    signature::hazmat::PrehashVerifier,
};
use sha2::{Digest, Sha256};

use crate::{AuthError, DispatchOutcome, RecoveredActorId};

sol! {
    /// Mirror of `OpenZeppelin` `WebAuthn.WebAuthnAuth` (verified against
    /// `openzeppelin-contracts/utils/cryptography/WebAuthn.sol`), used to
    /// ABI-decode the deployed `WebAuthnAuthenticator`'s `data` blob, which is
    /// `abi.encode(WebAuthn.WebAuthnAuth, bytes32 x, bytes32 y)`.
    ///
    /// Field order and types must match positionally — ABI decoding is positional.
    /// Note this is OZ's layout (`r, s` first, as `bytes32`), which deliberately
    /// differs from Coinbase/Daimo `webauthn-sol` (`authenticatorData,
    /// clientDataJSON, ..., uint256 r, s`); the deployed contract imports OZ.
    struct WebAuthnAuth {
        bytes32 r;
        bytes32 s;
        uint256 challengeIndex;
        uint256 typeIndex;
        bytes authenticatorData;
        string clientDataJSON;
    }
}

/// The expected `"type"` member of a `WebAuthn` `clientDataJSON` (21 bytes).
const WEBAUTHN_TYPE: &[u8] = b"\"type\":\"webauthn.get\"";
/// Prefix of the `"challenge"` member of a `WebAuthn` `clientDataJSON`.
const WEBAUTHN_CHALLENGE_PREFIX: &[u8] = b"\"challenge\":\"";
/// `WebAuthn` authenticator-data flag bits.
const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_BACKUP_ELIGIBLE: u8 = 0x08;
const FLAG_BACKUP_STATE: u8 = 0x10;

/// Enshrined dispatch over the EIP-8130 canonical authenticator set.
///
/// Stateless: performs no storage reads and runs no EVM. See the crate docs for
/// the enshrine-vs-precompile distinction and the parity requirement against the
/// deployed authenticator contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthenticatorDispatch;

impl AuthenticatorDispatch {
    /// Authenticate `data` for `authenticator` against `hash`, returning the
    /// resolved actor (or a [`DispatchOutcome::Delegated`] obligation for the
    /// delegate authenticator).
    ///
    /// `hash` is the context-appropriate signing hash (sender, payer, or config
    /// change digest); the caller computes it. For the native secp256k1 path the
    /// caller passes [`Eip8130Constants::K1_AUTHENTICATOR`] as `authenticator`
    /// with the raw 65-byte signature as `data`.
    ///
    /// Routes by authenticator address. The delegate authenticator is dispatched
    /// structurally only (see [`Self::delegate`]): it never verifies the nested
    /// signature here, so this routes one level deep and single-hop is enforced by
    /// [`Self::delegate`] plus the authorize layer's own guard before it re-enters
    /// the full auth path against the delegate account.
    pub fn authenticate(
        hash: B256,
        authenticator: Address,
        data: &[u8],
    ) -> Result<DispatchOutcome, AuthError> {
        // `address(0)` is the empty / "no actor configured" sentinel and is never
        // a valid authenticator selector; it falls through to `NotCanonical`.
        //
        // secp256k1 is the protocol-reserved native k1 sentinel (`address(1)`);
        // there is no deployed secp256k1 authenticator contract.
        if authenticator == Eip8130Constants::K1_AUTHENTICATOR {
            return Ok(DispatchOutcome::Authenticated { actor_id: Self::ecrecover(hash, data)? });
        }
        if authenticator == Eip8130Contracts::P256_AUTHENTICATOR {
            return Ok(DispatchOutcome::Authenticated { actor_id: Self::p256(hash, data)? });
        }
        if authenticator == Eip8130Contracts::WEBAUTHN_AUTHENTICATOR {
            return Ok(DispatchOutcome::Authenticated { actor_id: Self::webauthn(hash, data)? });
        }
        if authenticator == Eip8130Contracts::DELEGATE_AUTHENTICATOR {
            return Self::delegate(data);
        }
        Err(AuthError::NotCanonical(authenticator))
    }

    /// `actorId = bytes32(bytes20(address))`: the 20 address bytes left-aligned,
    /// right-padded with zeros.
    fn address_actor_id(address: Address) -> B256 {
        let mut id = [0u8; 32];
        id[..20].copy_from_slice(address.as_slice());
        B256::from(id)
    }

    /// Native secp256k1 ecrecover for the `K1_AUTHENTICATOR` sentinel, resolving
    /// `actorId = bytes32(bytes20(recovered))`. Delegates to
    /// [`RecoveredActorId::recover_k1`] — the single source of truth for the k1
    /// recovery (`v in {27, 28}`, EIP-2 low-`s`) — so the dispatch path and the
    /// proof-of-recovery token cannot drift from one another or from the
    /// deployed `AccountConfiguration` reference.
    fn ecrecover(hash: B256, data: &[u8]) -> Result<B256, AuthError> {
        Ok(RecoveredActorId::recover_k1(hash, data)?.actor_id())
    }

    /// P-256 raw authenticator. `data = r(32) || s(32) || x(32) || y(32) || pre_hash(1)`
    /// (exactly 129 bytes). `actorId = keccak256(x || y)`.
    ///
    /// The trailing `pre_hash` byte (`data[128]`) is **reserved**: the deployed
    /// `P256Authenticator` requires it to be present (it enforces `length == 129`)
    /// but never reads it — it exists only so the contract wire format matches the
    /// native-authenticator form. We mirror that exactly: require the byte, ignore
    /// its value, and verify directly over `hash`. If the contract ever begins
    /// interpreting `pre_hash`, this must change in lockstep to preserve parity.
    fn p256(hash: B256, data: &[u8]) -> Result<B256, AuthError> {
        if data.len() != 129 {
            return Err(AuthError::MalformedAuth);
        }
        let (r, s, x, y) = (&data[0..32], &data[32..64], &data[64..96], &data[96..128]);
        Self::p256_verify(hash.as_slice(), r, s, x, y)?;
        Ok(keccak256([x, y].concat()))
    }

    /// Verify a P-256 signature `(r, s)` over `prehash` for public key `(x, y)`.
    /// Enforces low-`s` to match `OpenZeppelin` `P256.verify` (malleability check).
    fn p256_verify(
        prehash: &[u8],
        r: &[u8],
        s: &[u8],
        x: &[u8],
        y: &[u8],
    ) -> Result<(), AuthError> {
        let mut sec1 = [0u8; 65];
        sec1[0] = 0x04;
        sec1[1..33].copy_from_slice(x);
        sec1[33..65].copy_from_slice(y);
        let key =
            P256VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| AuthError::InvalidPublicKey)?;

        let mut rs = [0u8; 64];
        rs[..32].copy_from_slice(r);
        rs[32..].copy_from_slice(s);
        let signature = P256Signature::from_slice(&rs).map_err(|_| AuthError::InvalidSignature)?;
        if signature.normalize_s().is_some() {
            return Err(AuthError::InvalidSignature);
        }
        key.verify_prehash(prehash, &signature).map_err(|_| AuthError::InvalidSignature)
    }

    /// `WebAuthn` authenticator. `data = abi.encode(WebAuthnAuth, x, y)`.
    /// `actorId = keccak256(x || y)`. Mirrors `OpenZeppelin` `WebAuthn.verify`
    /// with `requireUV = false` (as the deployed `WebAuthnAuthenticator`).
    fn webauthn(hash: B256, data: &[u8]) -> Result<B256, AuthError> {
        let (auth, x, y) = <(WebAuthnAuth, B256, B256)>::abi_decode_params(data)
            .map_err(|_| AuthError::MalformedAuth)?;

        let auth_data = auth.authenticatorData.as_ref();
        let client_json = auth.clientDataJSON.as_bytes();

        // 37-byte minimum authenticator data (32 rpIdHash + 1 flags + 4 counter).
        if auth_data.len() <= 36 {
            return Err(AuthError::InvalidSignature);
        }
        // Step 11: `"type":"webauthn.get"` at typeIndex.
        Self::contains_at(client_json, &auth.typeIndex, WEBAUTHN_TYPE)?;
        // Step 12: `"challenge":"<base64url(hash)>"` at challengeIndex.
        let mut expected = Vec::with_capacity(WEBAUTHN_CHALLENGE_PREFIX.len() + 44);
        expected.extend_from_slice(WEBAUTHN_CHALLENGE_PREFIX);
        expected.extend_from_slice(URL_SAFE_NO_PAD.encode(hash.as_slice()).as_bytes());
        expected.push(b'"');
        Self::contains_at(client_json, &auth.challengeIndex, &expected)?;
        // Step 16: User Present bit must be set. Step 17 (UV) skipped (requireUV = false).
        let flags = auth_data[32];
        if flags & FLAG_USER_PRESENT != FLAG_USER_PRESENT {
            return Err(AuthError::InvalidSignature);
        }
        // Backup state consistency: BS=1 requires BE=1.
        if flags & FLAG_BACKUP_ELIGIBLE != FLAG_BACKUP_ELIGIBLE && flags & FLAG_BACKUP_STATE != 0 {
            return Err(AuthError::InvalidSignature);
        }
        // Step 19-20: P-256 verify over sha256(authenticatorData || sha256(clientDataJSON)).
        let client_hash = Sha256::digest(client_json);
        let mut signed = Sha256::new();
        signed.update(auth_data);
        signed.update(client_hash);
        let signed = signed.finalize();

        Self::p256_verify(
            signed.as_slice(),
            auth.r.as_slice(),
            auth.s.as_slice(),
            x.as_slice(),
            y.as_slice(),
        )?;
        Ok(keccak256([x.as_slice(), y.as_slice()].concat()))
    }

    /// Asserts `haystack[index..index + needle.len()] == needle`, rejecting an
    /// out-of-range index or mismatch.
    fn contains_at(haystack: &[u8], index: &U256, needle: &[u8]) -> Result<(), AuthError> {
        let index = Self::index(index)?;
        let end = index.checked_add(needle.len()).ok_or(AuthError::MalformedAuth)?;
        if haystack.len() < end || &haystack[index..end] != needle {
            return Err(AuthError::InvalidSignature);
        }
        Ok(())
    }

    /// Narrows a `clientDataJSON` index from `uint256` to `usize`, rejecting any
    /// value that does not fit the platform pointer width (it cannot be a valid
    /// offset into the blob anyway). Correct on 32- and 64-bit targets.
    fn index(value: &U256) -> Result<usize, AuthError> {
        usize::try_from(*value).map_err(|_| AuthError::MalformedAuth)
    }

    /// Delegate authenticator. `data = delegate_account(20) || nested_auth`, where
    /// `nested_auth = nested_authenticator(20) || nested_data`.
    ///
    /// Structural only, mirroring the deployed `DelegateAuthenticator`: it derives
    /// `actorId = bytes32(bytes20(delegate))`, enforces the single-hop rule
    /// (`nested_authenticator != DELEGATE_AUTHENTICATOR`), and surfaces a
    /// [`DispatchOutcome::Delegated`] obligation. It does **not** verify the nested
    /// signature or resolve the nested actor — the deployed contract likewise only
    /// calls `ACCOUNT_CONFIGURATION.authenticateActor(delegate, ...)`, which the
    /// authorize stage mirrors by re-entering the full auth path against the
    /// delegate account. Verifying here would be a redundant second ecrecover and
    /// would skip the delegate account's inline default-EOA key.
    fn delegate(data: &[u8]) -> Result<DispatchOutcome, AuthError> {
        if data.len() < 40 {
            return Err(AuthError::MalformedAuth);
        }
        let delegate_account = Address::from_slice(&data[..20]);
        let nested_authenticator = Address::from_slice(&data[20..40]);

        // Only one delegation hop is permitted (depth-1).
        if nested_authenticator == Eip8130Contracts::DELEGATE_AUTHENTICATOR {
            return Err(AuthError::NestedDelegate);
        }

        Ok(DispatchOutcome::Delegated {
            actor_id: Self::address_actor_id(delegate_account),
            delegate_account,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
    use alloy_sol_types::SolValue;
    use k256::ecdsa::{Signature as K256Signature, SigningKey as K256SigningKey};
    use p256::ecdsa::{
        Signature as P256Sig, SigningKey as P256SigningKey, signature::hazmat::PrehashSigner,
    };
    use sha2::{Digest, Sha256};

    use super::*;

    const HASH: B256 = B256::repeat_byte(0x42);

    fn k1_key() -> K256SigningKey {
        K256SigningKey::from_slice(&[0x11u8; 32]).unwrap()
    }

    fn k1_address(key: &K256SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    /// 65-byte `r || s || v` signature over `HASH`, `v` in `{27, 28}` (low-s).
    fn k1_sig(key: &K256SigningKey, hash: B256) -> [u8; 65] {
        let (sig, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = recid.to_byte() + 27;
        out
    }

    fn p256_key() -> P256SigningKey {
        P256SigningKey::from_slice(&[0x22u8; 32]).unwrap()
    }

    fn p256_xy(key: &P256SigningKey) -> ([u8; 32], [u8; 32]) {
        let point = key.verifying_key().to_encoded_point(false);
        let bytes = point.as_bytes();
        (bytes[1..33].try_into().unwrap(), bytes[33..65].try_into().unwrap())
    }

    fn p256_sign(key: &P256SigningKey, prehash: &[u8]) -> P256Sig {
        let sig: P256Sig = key.sign_prehash(prehash).unwrap();
        sig.normalize_s().unwrap_or(sig)
    }

    /// `data = r || s || x || y || pre_hash` for the P-256 authenticator.
    fn p256_blob(key: &P256SigningKey, hash: B256) -> (Vec<u8>, B256) {
        let (x, y) = p256_xy(key);
        let sig = p256_sign(key, hash.as_slice());
        let mut data = Vec::with_capacity(129);
        data.extend_from_slice(&sig.to_bytes());
        data.extend_from_slice(&x);
        data.extend_from_slice(&y);
        data.push(0);
        (data, keccak256([x.as_slice(), y.as_slice()].concat()))
    }

    #[test]
    fn ecrecover_native_resolves_eoa_actor_id() {
        let key = k1_key();
        let out = AuthenticatorDispatch::authenticate(
            HASH,
            Eip8130Constants::K1_AUTHENTICATOR,
            &k1_sig(&key, HASH),
        )
        .unwrap();
        let expected = AuthenticatorDispatch::address_actor_id(k1_address(&key));
        assert_eq!(out, DispatchOutcome::Authenticated { actor_id: expected });
    }

    #[test]
    fn ecrecover_requires_v_27_or_28() {
        let key = k1_key();
        let mut sig = k1_sig(&key, HASH);
        sig[64] -= 27; // 0 or 1: invalid for the EVM ecrecover sentinel.
        assert_eq!(
            AuthenticatorDispatch::authenticate(HASH, Eip8130Constants::K1_AUTHENTICATOR, &sig,),
            Err(AuthError::InvalidSignature),
        );
    }

    #[test]
    fn ecrecover_rejects_high_s() {
        // EIP-2 low-s: the malleable upper-half-s counterpart of a valid signature
        // (negate s, flip the recovery parity) recovers the same signer but MUST be
        // rejected so the transaction id cannot be malleated.
        let key = k1_key();
        let (sig, recid) = key.sign_prehash_recoverable(HASH.as_slice()).unwrap();
        let s_high = -*sig.s();
        let high = K256Signature::from_scalars(sig.r().to_bytes(), s_high.to_bytes()).unwrap();
        let mut bytes = [0u8; 65];
        bytes[..64].copy_from_slice(&high.to_bytes());
        bytes[64] = (recid.to_byte() ^ 1) + 27;
        assert_eq!(
            AuthenticatorDispatch::authenticate(HASH, Eip8130Constants::K1_AUTHENTICATOR, &bytes,),
            Err(AuthError::InvalidSignature),
        );
    }

    #[test]
    fn ecrecover_rejects_wrong_length() {
        assert_eq!(
            AuthenticatorDispatch::authenticate(
                HASH,
                Eip8130Constants::K1_AUTHENTICATOR,
                &[0u8; 64],
            ),
            Err(AuthError::MalformedAuth),
        );
    }

    #[test]
    fn p256_resolves_keccak_xy_actor_id() {
        let key = p256_key();
        let (data, expected) = p256_blob(&key, HASH);
        let out =
            AuthenticatorDispatch::authenticate(HASH, Eip8130Contracts::P256_AUTHENTICATOR, &data)
                .unwrap();
        assert_eq!(out, DispatchOutcome::Authenticated { actor_id: expected });
    }

    #[test]
    fn p256_rejects_tampered_hash() {
        let key = p256_key();
        let (data, _) = p256_blob(&key, HASH);
        assert_eq!(
            AuthenticatorDispatch::authenticate(
                B256::repeat_byte(0x99),
                Eip8130Contracts::P256_AUTHENTICATOR,
                &data,
            ),
            Err(AuthError::InvalidSignature),
        );
    }

    #[test]
    fn p256_rejects_wrong_length() {
        assert_eq!(
            AuthenticatorDispatch::authenticate(
                HASH,
                Eip8130Contracts::P256_AUTHENTICATOR,
                &[0u8; 128],
            ),
            Err(AuthError::MalformedAuth),
        );
    }

    /// Builds an OZ-compatible `WebAuthn` blob (`abi.encode(WebAuthnAuth, x, y)`)
    /// that authenticates `hash`.
    fn webauthn_blob(key: &P256SigningKey, hash: B256) -> (Vec<u8>, B256) {
        let (x, y) = p256_xy(key);
        let challenge = URL_SAFE_NO_PAD.encode(hash.as_slice());
        let client_json = format!(
            "{{\"type\":\"webauthn.get\",\"challenge\":\"{challenge}\",\"origin\":\"https://base.org\"}}"
        );
        let type_index = client_json.find("\"type\":\"webauthn.get\"").unwrap();
        let challenge_index = client_json.find("\"challenge\":\"").unwrap();

        // 37 bytes: 32 rpIdHash + flags (UP set) + 4-byte counter.
        let mut auth_data = vec![0xAAu8; 32];
        auth_data.push(FLAG_USER_PRESENT);
        auth_data.extend_from_slice(&[0, 0, 0, 1]);

        let mut signed = Sha256::new();
        signed.update(&auth_data);
        signed.update(Sha256::digest(client_json.as_bytes()));
        let sig = p256_sign(key, &signed.finalize());

        let auth = WebAuthnAuth {
            r: B256::from_slice(&sig.to_bytes()[..32]),
            s: B256::from_slice(&sig.to_bytes()[32..]),
            challengeIndex: U256::from(challenge_index),
            typeIndex: U256::from(type_index),
            authenticatorData: Bytes::from(auth_data),
            clientDataJSON: client_json,
        };
        let data = (auth, B256::from(x), B256::from(y)).abi_encode_params();
        (data, keccak256([x.as_slice(), y.as_slice()].concat()))
    }

    #[test]
    fn webauthn_resolves_keccak_xy_actor_id() {
        let key = p256_key();
        let (data, expected) = webauthn_blob(&key, HASH);
        let out = AuthenticatorDispatch::authenticate(
            HASH,
            Eip8130Contracts::WEBAUTHN_AUTHENTICATOR,
            &data,
        )
        .unwrap();
        assert_eq!(out, DispatchOutcome::Authenticated { actor_id: expected });
    }

    #[test]
    fn webauthn_rejects_wrong_challenge() {
        let key = p256_key();
        // Blob authenticates HASH, but we dispatch against a different hash, so the
        // embedded challenge no longer matches the expected base64url(hash).
        let (data, _) = webauthn_blob(&key, HASH);
        assert_eq!(
            AuthenticatorDispatch::authenticate(
                B256::repeat_byte(0x01),
                Eip8130Contracts::WEBAUTHN_AUTHENTICATOR,
                &data,
            ),
            Err(AuthError::InvalidSignature),
        );
    }

    #[test]
    fn delegate_surfaces_obligation_without_verifying_nested() {
        // Dispatch is structural only: it surfaces the delegate account without
        // verifying the nested signature (the authorize stage does that via the
        // full `authenticateActor` path). A garbage nested signature still yields
        // the obligation here — a canonical nested authenticator is all that is
        // structurally required.
        let delegate_account = address!("0x00000000000000000000000000000000000000bb");

        let mut data = Vec::new();
        data.extend_from_slice(delegate_account.as_slice());
        data.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        data.extend_from_slice(&[0u8; 65]);

        let out = AuthenticatorDispatch::authenticate(
            HASH,
            Eip8130Contracts::DELEGATE_AUTHENTICATOR,
            &data,
        )
        .unwrap();
        assert_eq!(
            out,
            DispatchOutcome::Delegated {
                actor_id: AuthenticatorDispatch::address_actor_id(delegate_account),
                delegate_account,
            }
        );
    }

    #[test]
    fn delegate_rejects_nested_delegate() {
        let delegate_account = address!("0x00000000000000000000000000000000000000bb");
        let mut data = Vec::new();
        data.extend_from_slice(delegate_account.as_slice());
        // Nested authenticator is the delegate authenticator itself (depth-2).
        data.extend_from_slice(Eip8130Contracts::DELEGATE_AUTHENTICATOR.as_slice());
        data.extend_from_slice(&[0u8; 65]);
        assert_eq!(
            AuthenticatorDispatch::authenticate(
                HASH,
                Eip8130Contracts::DELEGATE_AUTHENTICATOR,
                &data,
            ),
            Err(AuthError::NestedDelegate),
        );
    }

    #[test]
    fn delegate_rejects_short_data() {
        // Below the 40-byte delegate(20) || nested_authenticator(20) prefix.
        let data = [0u8; 39];
        assert_eq!(
            AuthenticatorDispatch::authenticate(
                HASH,
                Eip8130Contracts::DELEGATE_AUTHENTICATOR,
                &data,
            ),
            Err(AuthError::MalformedAuth),
        );
    }

    #[test]
    fn rejects_zero_authenticator_selector() {
        // `address(0)` is the empty / "no actor configured" sentinel, never a
        // valid authenticator selector.
        assert_eq!(
            AuthenticatorDispatch::authenticate(HASH, Address::ZERO, &[0u8; 65]),
            Err(AuthError::NotCanonical(Address::ZERO)),
        );
    }

    #[test]
    fn rejects_non_canonical_authenticator() {
        let authenticator = address!("0x00000000000000000000000000000000deadbeef");
        assert_eq!(
            AuthenticatorDispatch::authenticate(HASH, authenticator, &[0u8; 65]),
            Err(AuthError::NotCanonical(authenticator)),
        );
    }
}
