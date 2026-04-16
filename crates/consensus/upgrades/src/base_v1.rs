//! Module containing [`TxDeposit`] builders for the Base V1 network upgrade.
//!
//! Deploys the EIP-8130 Account Abstraction system contracts at hardfork
//! activation via deposit transactions. Follows the same pattern as
//! [`super::Jovian`]: deployer addresses at `0x4210…0008` through `0x…000d`,
//! creation bytecodes loaded from hex files, deterministic addresses from
//! `deployer.create(0)`.
//!
//! # Contract deployment order
//!
//! 1. K1Verifier            (no constructor args)
//! 2. P256Verifier          (no constructor args)
//! 3. WebAuthnVerifier      (no constructor args)
//! 4. AccountConfiguration  (constructor: k1, p256, webauthn, delegate=address(0))
//! 5. DelegateVerifier      (constructor: accountConfiguration)
//! 6. DefaultAccount        (constructor: accountConfiguration)

use alloc::{string::String, vec::Vec};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, hex};
use base_alloy_consensus::{TxDeposit, UpgradeDepositSource};
use base_protocol::Deployers;

use crate::Hardfork;

/// The Base V1 network upgrade transactions.
#[derive(Debug, Default, Clone, Copy)]
pub struct BaseV1;

impl BaseV1 {
    /// K1 Verifier deployment source hash.
    pub fn deploy_k1_verifier_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Base V1: K1 Verifier Deployment") }
            .source_hash()
    }

    /// P256 Verifier deployment source hash.
    pub fn deploy_p256_verifier_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Base V1: P256 Verifier Deployment") }
            .source_hash()
    }

    /// WebAuthn Verifier deployment source hash.
    pub fn deploy_webauthn_verifier_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Base V1: WebAuthn Verifier Deployment") }
            .source_hash()
    }

    /// Account Configuration deployment source hash.
    pub fn deploy_account_configuration_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Base V1: Account Configuration Deployment") }
            .source_hash()
    }

    /// Delegate Verifier deployment source hash.
    pub fn deploy_delegate_verifier_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Base V1: Delegate Verifier Deployment") }
            .source_hash()
    }

    /// Default Account deployment source hash.
    pub fn deploy_default_account_source() -> B256 {
        UpgradeDepositSource { intent: String::from("Base V1: Default Account Deployment") }
            .source_hash()
    }

    /// K1 Verifier deployed address (`deployer.create(0)`).
    pub fn k1_verifier_address() -> Address {
        Deployers::BASE_V1_K1_VERIFIER.create(0)
    }

    /// P256 Verifier deployed address.
    pub fn p256_verifier_address() -> Address {
        Deployers::BASE_V1_P256_VERIFIER.create(0)
    }

    /// WebAuthn Verifier deployed address.
    pub fn webauthn_verifier_address() -> Address {
        Deployers::BASE_V1_WEBAUTHN_VERIFIER.create(0)
    }

    /// Account Configuration deployed address.
    pub fn account_configuration_address() -> Address {
        Deployers::BASE_V1_ACCOUNT_CONFIGURATION.create(0)
    }

    /// Delegate Verifier deployed address.
    pub fn delegate_verifier_address() -> Address {
        Deployers::BASE_V1_DELEGATE_VERIFIER.create(0)
    }

    /// Default Account deployed address.
    pub fn default_account_address() -> Address {
        Deployers::BASE_V1_DEFAULT_ACCOUNT.create(0)
    }

    fn k1_verifier_bytecode() -> Bytes {
        hex::decode(include_str!("./bytecode/base-v1-k1-verifier-deployment.hex").replace('\n', ""))
            .expect("valid hex")
            .into()
    }

    fn p256_verifier_bytecode() -> Bytes {
        hex::decode(
            include_str!("./bytecode/base-v1-p256-verifier-deployment.hex").replace('\n', ""),
        )
        .expect("valid hex")
        .into()
    }

    fn webauthn_verifier_bytecode() -> Bytes {
        hex::decode(
            include_str!("./bytecode/base-v1-web-authn-verifier-deployment.hex").replace('\n', ""),
        )
        .expect("valid hex")
        .into()
    }

    fn account_configuration_bytecode() -> Bytes {
        let base = hex::decode(
            include_str!("./bytecode/base-v1-account-configuration-deployment.hex")
                .replace('\n', ""),
        )
        .expect("valid hex");

        // ABI-encode constructor args: (k1, p256Raw, p256WebAuthn, delegate)
        // AccountConfiguration is deployed with delegate = address(0) to break
        // the circular dependency (DelegateVerifier needs AccountConfiguration).
        let k1 = Self::k1_verifier_address();
        let p256 = Self::p256_verifier_address();
        let webauthn = Self::webauthn_verifier_address();
        let delegate = Address::ZERO;

        let mut input = base;
        input.extend_from_slice(k1.into_word().as_slice());
        input.extend_from_slice(p256.into_word().as_slice());
        input.extend_from_slice(webauthn.into_word().as_slice());
        input.extend_from_slice(delegate.into_word().as_slice());
        input.into()
    }

    fn delegate_verifier_bytecode() -> Bytes {
        let base = hex::decode(
            include_str!("./bytecode/base-v1-delegate-verifier-deployment.hex").replace('\n', ""),
        )
        .expect("valid hex");

        let account_config = Self::account_configuration_address();
        let mut input = base;
        input.extend_from_slice(account_config.into_word().as_slice());
        input.into()
    }

    fn default_account_bytecode() -> Bytes {
        let base = hex::decode(
            include_str!("./bytecode/base-v1-default-account-deployment.hex").replace('\n', ""),
        )
        .expect("valid hex");

        let account_config = Self::account_configuration_address();
        let mut input = base;
        input.extend_from_slice(account_config.into_word().as_slice());
        input.into()
    }

    /// Returns the list of [`TxDeposit`]s for the network upgrade.
    pub fn deposits() -> impl Iterator<Item = TxDeposit> {
        ([
            TxDeposit {
                source_hash: Self::deploy_k1_verifier_source(),
                from: Deployers::BASE_V1_K1_VERIFIER,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 200_000,
                is_system_transaction: false,
                input: Self::k1_verifier_bytecode(),
            },
            TxDeposit {
                source_hash: Self::deploy_p256_verifier_source(),
                from: Deployers::BASE_V1_P256_VERIFIER,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 800_000,
                is_system_transaction: false,
                input: Self::p256_verifier_bytecode(),
            },
            TxDeposit {
                source_hash: Self::deploy_webauthn_verifier_source(),
                from: Deployers::BASE_V1_WEBAUTHN_VERIFIER,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 1_100_000,
                is_system_transaction: false,
                input: Self::webauthn_verifier_bytecode(),
            },
            TxDeposit {
                source_hash: Self::deploy_account_configuration_source(),
                from: Deployers::BASE_V1_ACCOUNT_CONFIGURATION,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 2_000_000,
                is_system_transaction: false,
                input: Self::account_configuration_bytecode(),
            },
            TxDeposit {
                source_hash: Self::deploy_delegate_verifier_source(),
                from: Deployers::BASE_V1_DELEGATE_VERIFIER,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 200_000,
                is_system_transaction: false,
                input: Self::delegate_verifier_bytecode(),
            },
            TxDeposit {
                source_hash: Self::deploy_default_account_source(),
                from: Deployers::BASE_V1_DEFAULT_ACCOUNT,
                to: TxKind::Create,
                mint: 0,
                value: U256::ZERO,
                gas_limit: 500_000,
                is_system_transaction: false,
                input: Self::default_account_bytecode(),
            },
        ])
        .into_iter()
    }
}

impl Hardfork for BaseV1 {
    fn txs(&self) -> impl Iterator<Item = Bytes> + '_ {
        Self::deposits().map(|tx| {
            let mut encoded = Vec::new();
            tx.encode_2718(&mut encoded);
            Bytes::from(encoded)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployed_addresses_are_deterministic() {
        assert_eq!(
            BaseV1::k1_verifier_address(),
            "0x5Be482Da3E457aB3b439B184532224EC42c6b8Db"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
        assert_eq!(
            BaseV1::p256_verifier_address(),
            "0x6751c7ED0C58319e75437f8E6Dafa2d7F6b8306F"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
        assert_eq!(
            BaseV1::webauthn_verifier_address(),
            "0x3572bb3F611a40DDcA70e5b55Cc797D58357AD44"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
        assert_eq!(
            BaseV1::account_configuration_address(),
            "0xf946601D5424118A4e4054BB0B13133f216b4FeE"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
        assert_eq!(
            BaseV1::delegate_verifier_address(),
            "0xc758A89C53542164aaB7f6439e8c8cAcf628fF62"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
        assert_eq!(
            BaseV1::default_account_address(),
            "0xAb4eE49EE97e49807e180BD5Fb9D9F35783b84F2"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
    }

    #[test]
    fn base_v1_has_six_deposits() {
        assert_eq!(BaseV1::deposits().count(), 6);
    }

    #[test]
    fn bytecodes_are_non_empty() {
        for (i, tx) in BaseV1::deposits().enumerate() {
            assert!(!tx.input.is_empty(), "deposit {i} has empty bytecode");
        }
    }
}
