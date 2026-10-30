use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, U256, address, keccak256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue, sol};
use async_trait::async_trait;

use super::Payload;
use crate::{
    BaselineError, Result,
    rpc::{BaseFeeExt, RpcResultExt, create_wallet_provider},
    workload::{
        SeededRng,
        chain_prep::{ChainPrepContext, prep_submission_max_fee},
    },
};

sol! {
    /// ERC-4337 `handleOps` stand-in: per op, checks an ECDSA signature over
    /// `keccak256(abi.encode(sender, to, salt))`, increments the sender's nonce, credits `to`
    /// one unit, and emits one event. `padding` only adds calldata. Nonces are not checked,
    /// so signed ops replay; it holds nothing of value.
    ///
    /// solc 0.8.28, optimizer 200 runs, no CBOR metadata:
    /// ```solidity
    /// contract AaBundle {
    ///     struct Op { address sender; address to; uint256 salt; uint8 v; bytes32 r; bytes32 s; }
    ///     mapping(address => uint256) public nonces;
    ///     mapping(address => uint256) public balanceOf;
    ///     event UserOperationEvent(
    ///         bytes32 indexed opHash, address indexed sender, address indexed to, uint256 nonce);
    ///
    ///     function handleOps(Op[] calldata ops, bytes calldata) external {
    ///         for (uint256 i; i < ops.length; ++i) {
    ///             Op calldata op = ops[i];
    ///             bytes32 opHash = keccak256(abi.encode(op.sender, op.to, op.salt));
    ///             address signer = ecrecover(opHash, op.v, op.r, op.s);
    ///             require(signer != address(0) && signer == op.sender, "bad signature");
    ///             uint256 nonce = nonces[op.sender]++;
    ///             balanceOf[op.to] += 1;
    ///             emit UserOperationEvent(opHash, op.sender, op.to, nonce);
    ///         }
    ///     }
    /// }
    /// ```
    #[sol(bytecode = "6080604052348015600e575f5ffd5b506104a18061001c5f395ff3fe608060405234801561000f575f5ffd5b506004361061003f575f3560e01c806370a0823114610043578063796af393146100745780637ecebe0014610089575b5f5ffd5b61006261005136600461031b565b60016020525f908152604090205481565b60405190815260200160405180910390f35b61008761008236600461038d565b6100a8565b005b61006261009736600461031b565b5f6020819052908152604090205481565b5f5b8381101561031457368585838181106100c5576100c5610428565b60c0029190910191505f90506100de602083018361031b565b6100ee604084016020850161031b565b604080516001600160a01b0393841660208201529290911682820152830135606082015260800160408051601f19818403018152919052805160209091012090505f600182610143608086016060870161043c565b604080515f8152602081018083529390935260ff90911690820152608080860135606083015260a08087013591830191909152016020604051602081039080840390855afa158015610197573d5f5f3e3d5ffd5b5050604051601f1901519150506001600160a01b038116158015906101d957506101c4602084018461031b565b6001600160a01b0316816001600160a01b0316145b6102195760405162461bcd60e51b815260206004820152600d60248201526c626164207369676e617475726560981b604482015260640160405180910390fd5b5f8080610229602087018761031b565b6001600160a01b0316815260208101919091526040015f90812080549161024f83610470565b9091555090506001805f610269604088016020890161031b565b6001600160a01b03166001600160a01b031681526020019081526020015f205f8282546102969190610488565b909155506102ac9050604085016020860161031b565b6001600160a01b03166102c2602086018661031b565b6001600160a01b0316847fda3a449beb78d554621c12bdcf266f2f7f007f00ccf51f7f078b9e0bfbe5e1b1846040516102fd91815260200190565b60405180910390a4505050508060010190506100aa565b5050505050565b5f6020828403121561032b575f5ffd5b81356001600160a01b0381168114610341575f5ffd5b9392505050565b5f5f83601f840112610358575f5ffd5b50813567ffffffffffffffff81111561036f575f5ffd5b602083019150836020828501011115610386575f5ffd5b9250929050565b5f5f5f5f604085870312156103a0575f5ffd5b843567ffffffffffffffff8111156103b6575f5ffd5b8501601f810187136103c6575f5ffd5b803567ffffffffffffffff8111156103dc575f5ffd5b87602060c0830284010111156103f0575f5ffd5b60209182019550935085013567ffffffffffffffff811115610410575f5ffd5b61041c87828801610348565b95989497509550505050565b634e487b7160e01b5f52603260045260245ffd5b5f6020828403121561044c575f5ffd5b813560ff81168114610341575f5ffd5b634e487b7160e01b5f52601160045260245ffd5b5f600182016104815761048161045c565b5060010190565b8082018082111561049b5761049b61045c565b9291505056")]
    contract AaBundle {
        struct Op { address sender; address to; uint256 salt; uint8 v; bytes32 r; bytes32 s; }
        function handleOps(Op[] calldata ops, bytes calldata padding) external;
    }
}

/// Deterministic deployment proxy, preinstalled on OP Stack chains and anvil.
const CREATE2_FACTORY: Address = address!("0x4e59b44847b379578588920cA78FbF26c0B4956C");
/// Op signers; each one's nonce slot exists after its first op, like a returning account.
const OP_SIGNERS: u64 = 32;
const GAS_PER_OP: u64 = 60_000;
const GAS_BASE: u64 = 40_000;
/// Covers the EIP-7623 calldata floor of 40 gas per non-zero byte.
const GAS_PER_PADDING_BYTE: u64 = 40;

/// Sends one `handleOps` bundle of signed ops to an [`AaBundle`] deployed during prepare.
#[derive(Debug, Clone)]
pub struct AaBundlePayload {
    ops_per_bundle: u32,
    padding_bytes: u32,
    signers: Vec<PrivateKeySigner>,
}

impl AaBundlePayload {
    /// Creates a payload sending `ops_per_bundle` ops and `padding_bytes` random bytes per tx.
    pub fn new(ops_per_bundle: u32, padding_bytes: u32) -> Self {
        let signers = (1..=OP_SIGNERS)
            .map(|i| PrivateKeySigner::from_bytes(&B256::from(U256::from(i))).expect("valid key"))
            .collect();
        Self { ops_per_bundle, padding_bytes, signers }
    }

    /// Address of the bundle contract.
    pub fn contract() -> Address {
        CREATE2_FACTORY.create2_from_code(B256::ZERO, &AaBundle::BYTECODE)
    }
}

#[async_trait]
impl Payload for AaBundlePayload {
    fn name(&self) -> &'static str {
        "aa_bundle"
    }

    fn uses_runner_recipient(&self) -> bool {
        false
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        let ops = (0..self.ops_per_bundle)
            .map(|_| {
                let signer = &self.signers[rng.gen_range(0..self.signers.len())];
                // Not seeded: calibration and reruns replay the seed, which would reuse these slots.
                let to = Address::random();
                let salt = U256::from_be_bytes(rng.gen_bytes::<32>());
                let hash = keccak256((signer.address(), to, salt).abi_encode());
                let sig = signer.sign_hash_sync(&hash).expect("local signing succeeds");
                AaBundle::Op {
                    sender: signer.address(),
                    to,
                    salt,
                    v: 27 + u8::from(sig.v()),
                    r: sig.r().into(),
                    s: sig.s().into(),
                }
            })
            .collect();
        let padding: Vec<u8> = (0..self.padding_bytes).map(|_| rng.random()).collect();
        let input = AaBundle::handleOpsCall { ops, padding: padding.into() }.abi_encode();
        TransactionRequest::default().with_to(Self::contract()).with_input(input).with_gas_limit(
            GAS_BASE
                + GAS_PER_OP * u64::from(self.ops_per_bundle)
                + GAS_PER_PADDING_BYTE * u64::from(self.padding_bytes),
        )
    }

    async fn prepare(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        let account = &ctx.accounts.accounts()[0];
        // The query node may lag the submission node, which confirms the deployment.
        let provider = create_wallet_provider(
            ctx.primary_submission_rpc.clone(),
            EthereumWallet::from(account.signer.clone()),
        );
        let contract = Self::contract();
        if !provider.get_code_at(contract).await.rpc("get aa_bundle code")?.is_empty() {
            return Ok(());
        }
        let nonce = provider
            .get_transaction_count(account.address)
            .pending()
            .await
            .rpc("get aa_bundle deployer nonce")?;
        let base_fee = ctx.client.get_base_fee().await?;
        let priority_fee = (base_fee / 10).max(1);
        let max_fee = prep_submission_max_fee(base_fee, priority_fee, ctx.max_gas_price);
        let tx = TransactionRequest::default()
            .with_to(CREATE2_FACTORY)
            .with_input([B256::ZERO.as_slice(), &AaBundle::BYTECODE].concat())
            .with_nonce(nonce)
            .with_gas_limit(500_000)
            .with_max_fee_per_gas(max_fee)
            .with_max_priority_fee_per_gas(priority_fee);
        provider
            .send_transaction(tx)
            .await
            .rpc("send aa_bundle deployment")?
            .with_timeout(Some(std::time::Duration::from_secs(120)))
            .get_receipt()
            .await
            .rpc("aa_bundle deployment receipt")?;
        ctx.outputs.needs_sender_refresh = true;
        // A concurrent run may have deployed it first, reverting this deployment.
        if provider.get_code_at(contract).await.rpc("get aa_bundle code")?.is_empty() {
            return Err(BaselineError::Transaction(format!(
                "aa_bundle contract missing at {contract} after deployment"
            )));
        }
        Ok(())
    }
}
