//! A verifier that accepts any proof, for forks that never see a real one.
//!
//! Source: `contracts/MockVerifier.sol`. The runtime next to it is what gets
//! installed; regenerate it after editing the source with
//!
//! ```text
//! forge inspect --use 0.8.15 --optimize --optimizer-runs 200 \
//!     contracts/MockVerifier.sol:MockVerifier deployedBytecode \
//!     | sed 's/^0x//' | tr -d '\n' > contracts/MockVerifier.runtime.hex
//! ```
//!
//! and the tests below check the result behaves as the source says.

use alloy_primitives::{Address, B256, Bytes, U256, hex};
use alloy_provider::{Provider, RootProvider};
use eyre::{Context, Result, ensure};

/// Storage slot of the real `Verifier.nullified` flag, which the mock keeps.
const NULLIFIED_SLOT: U256 = U256::ZERO;

/// Storage slot the mock reads its `AnchorStateRegistry` from. Unused by the
/// real verifiers, whose registry is an immutable.
const REGISTRY_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Deployed runtime of `contracts/MockVerifier.sol`.
pub(crate) fn runtime() -> Bytes {
    Bytes::from(
        hex::decode(include_str!("../contracts/MockVerifier.runtime.hex").trim())
            .expect("MockVerifier.runtime.hex must be valid hex"),
    )
}

/// What a verifier address held before the mock went in, so it can be put back.
#[derive(Debug, Clone)]
pub(crate) struct Original {
    address: Address,
    code: Bytes,
    registry_slot: B256,
}

/// Replaces the code at `verifier` with the mock, keeping its `nullified`
/// flag, and points the mock at `registry` for `nullify`'s guard.
pub(crate) async fn install(
    provider: &RootProvider,
    verifier: Address,
    registry: Address,
) -> Result<Original> {
    let code = provider
        .get_code_at(verifier)
        .await
        .with_context(|| format!("failed to read the code at verifier {verifier}"))?;
    ensure!(!code.is_empty(), "no contract at verifier {verifier}");
    let registry_slot = B256::from(
        provider
            .get_storage_at(verifier, REGISTRY_SLOT)
            .await
            .with_context(|| format!("failed to read slot 1 of verifier {verifier}"))?,
    );
    let nullified_before = provider.get_storage_at(verifier, NULLIFIED_SLOT).await?;

    set_code(provider, verifier, runtime()).await?;
    set_storage(provider, verifier, REGISTRY_SLOT, registry.into_word()).await?;

    // The mock must inherit the real verifier's state, not reset it: a verifier
    // nullified on the source chain has to stay nullified on the fork.
    let nullified_after = provider.get_storage_at(verifier, NULLIFIED_SLOT).await?;
    ensure!(
        nullified_after == nullified_before,
        "installing the mock changed verifier {verifier}'s nullified flag"
    );
    Ok(Original { address: verifier, code, registry_slot })
}

/// Restores what [`install`] replaced, leaving `nullified` as the mock left it,
/// and reads the code back rather than assuming the write landed.
pub(crate) async fn restore(provider: &RootProvider, original: &Original) -> Result<()> {
    let Original { address, code, registry_slot } = original;
    set_code(provider, *address, code.clone()).await?;
    set_storage(provider, *address, REGISTRY_SLOT, *registry_slot).await?;
    let after = provider.get_code_at(*address).await?;
    ensure!(after == *code, "verifier {address} did not read back as its own code");
    Ok(())
}

async fn set_code(provider: &RootProvider, address: Address, code: Bytes) -> Result<()> {
    provider
        .client()
        .request::<_, ()>("anvil_setCode", (address, code))
        .await
        .with_context(|| format!("anvil_setCode failed for {address}"))
}

async fn set_storage(
    provider: &RootProvider,
    address: Address,
    slot: U256,
    value: B256,
) -> Result<()> {
    let updated = provider
        .client()
        .request::<_, bool>("anvil_setStorageAt", (address, B256::from(slot), value))
        .await
        .with_context(|| format!("anvil_setStorageAt failed for {address}"))?;
    ensure!(updated, "anvil_setStorageAt returned false for {address}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_node_bindings::Anvil;
    use alloy_primitives::keccak256;
    use alloy_provider::Provider;
    use alloy_rpc_types_eth::TransactionRequest;

    use super::*;

    fn selector(signature: &str) -> [u8; 4] {
        keccak256(signature.as_bytes())[..4].try_into().expect("four bytes")
    }

    /// `verify(bytes,bytes32,bytes32)` with an empty proof.
    fn verify_calldata() -> Bytes {
        let mut data = selector("verify(bytes,bytes32,bytes32)").to_vec();
        // offset of `bytes` (0x60), two zero words, then a zero length.
        data.extend_from_slice(&U256::from(0x60).to_be_bytes::<32>());
        data.extend_from_slice(&[0u8; 32]);
        data.extend_from_slice(&[0u8; 32]);
        data.extend_from_slice(&[0u8; 32]);
        Bytes::from(data)
    }

    async fn call(provider: &RootProvider, to: Address, data: Bytes) -> Result<Bytes> {
        Ok(provider.call(TransactionRequest::default().to(to).input(data.into())).await?)
    }

    /// Answers every registry query from slot-0 flags so the guard can be
    /// steered: returns `true` for `isGameRegistered` / `isGameRespected` and
    /// `false` for the blacklist/retired checks. Hand-written: dispatches on
    /// the selector and returns 1 for the first two, 0 otherwise.
    fn friendly_registry_runtime() -> Bytes {
        let registered = selector("isGameRegistered(address)");
        let respected = selector("isGameRespected(address)");
        let mut code = vec![
            0x60, 0x00, 0x35, 0x60, 0xe0, 0x1c, // PUSH1 0 CALLDATALOAD PUSH1 224 SHR
            0x80, 0x63, // DUP1 PUSH4
        ];
        code.extend_from_slice(&registered);
        code.extend_from_slice(&[0x14, 0x61, 0x00, 0x00, 0x57]); // EQ PUSH2 <yes> JUMPI
        code.push(0x63); // PUSH4
        code.extend_from_slice(&respected);
        code.extend_from_slice(&[0x14, 0x61, 0x00, 0x00, 0x57]); // EQ PUSH2 <yes> JUMPI
        // no: return 0
        code.extend_from_slice(&[0x60, 0x00, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
        let yes = code.len();
        // yes: JUMPDEST return 1
        code.extend_from_slice(&[0x5b, 0x60, 0x01, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
        let yes_hi = u8::try_from(yes >> 8).expect("small");
        let yes_lo = u8::try_from(yes & 0xff).expect("small");
        // Patch both jump targets.
        for (index, window) in code.clone().windows(5).enumerate() {
            if window == [0x14, 0x61, 0x00, 0x00, 0x57] {
                code[index + 2] = yes_hi;
                code[index + 3] = yes_lo;
            }
        }
        Bytes::from(code)
    }

    #[tokio::test]
    async fn accepts_any_proof_until_nullified_and_keeps_the_real_guard() {
        let anvil = Anvil::new().spawn();
        let provider: RootProvider = RootProvider::new_http(anvil.endpoint_url());
        let verifier = Address::repeat_byte(0x42);
        let registry = Address::repeat_byte(0x43);
        let original_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
        set_code(&provider, verifier, original_code.clone()).await.expect("seed verifier");

        let original = install(&provider, verifier, registry).await.expect("install");

        // Any proof verifies while live.
        let verified = call(&provider, verifier, verify_calldata()).await.expect("verify");
        assert_eq!(U256::from_be_slice(&verified), U256::from(1));

        // The guard is real: with no registry code every query reverts or
        // returns nothing, so an unregistered caller cannot nullify.
        let nullify = Bytes::from(selector("nullify()").to_vec());
        assert!(call(&provider, verifier, nullify.clone()).await.is_err(), "guard must hold");

        // A registry that vouches for the caller lets `nullify` through, and
        // the flag lands in slot 0 where the real `Verifier` keeps it.
        set_code(&provider, registry, friendly_registry_runtime()).await.expect("registry");
        let sender = anvil.addresses()[0];
        let tx = TransactionRequest::default().from(sender).to(verifier).input(nullify.into());
        provider.send_transaction(tx).await.expect("send").get_receipt().await.expect("mined");
        let flag = provider.get_storage_at(verifier, NULLIFIED_SLOT).await.expect("slot 0");
        assert_eq!(flag, U256::from(1), "nullified is slot 0");

        // Once nullified, nothing verifies.
        assert!(call(&provider, verifier, verify_calldata()).await.is_err(), "Nullified()");
        let nullified = call(&provider, verifier, Bytes::from(selector("nullified()").to_vec()))
            .await
            .expect("nullified()");
        assert_eq!(U256::from_be_slice(&nullified), U256::from(1));

        // Restoring puts the original code back and keeps the flag, which is
        // the global side effect a real nullify has.
        restore(&provider, &original).await.expect("restore");
        assert_eq!(provider.get_code_at(verifier).await.expect("code"), original_code);
        assert_eq!(
            provider.get_storage_at(verifier, NULLIFIED_SLOT).await.expect("slot 0"),
            U256::from(1)
        );
        assert_eq!(
            provider.get_storage_at(verifier, REGISTRY_SLOT).await.expect("slot 1"),
            U256::ZERO,
            "slot 1 is put back"
        );
    }

    #[tokio::test]
    async fn install_inherits_an_already_nullified_verifier() {
        let anvil = Anvil::new().spawn();
        let provider: RootProvider = RootProvider::new_http(anvil.endpoint_url());
        let verifier = Address::repeat_byte(0x44);
        set_code(&provider, verifier, Bytes::from_static(&[0x00])).await.expect("seed");
        set_storage(&provider, verifier, NULLIFIED_SLOT, B256::with_last_byte(1))
            .await
            .expect("pre-nullify");

        install(&provider, verifier, Address::repeat_byte(0x45)).await.expect("install");

        assert!(call(&provider, verifier, verify_calldata()).await.is_err(), "still nullified");
    }
}
