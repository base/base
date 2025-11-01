mod common;

use alloy_consensus::Receipt;
use alloy_primitives::{address, b256, bytes, map::HashMap, Address, Bytes, B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::TransactionInput;
use base_reth_flashblocks_rpc::subscription::{Flashblock, Metadata};
use common::{create_base_flashblock, setup_node, ALICE, BOB, CHARLIE};
use op_alloy_network::Optimism;
use op_alloy_rpc_types::OpTransactionRequest;
use reth_optimism_primitives::OpReceipt;
use rollup_boost::ExecutionPayloadFlashblockDeltaV1;

const ERC20_ADDRESS: Address = address!("0x5FbDB2315678afecb367f032d93F642f64180aa3");

// 1. deployment: Alice deploys SimpleToken contract, receives 1000 tokens
const DEPLOYMENT_TX: Bytes = bytes!("0x02f905df83014a34800184773594018305d2228080b905876080604052348015600e575f5ffd5b506103e86001819055506103e85f5f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f208190555061051f806100685f395ff3fe608060405234801561000f575f5ffd5b506004361061004a575f3560e01c806318160ddd1461004e57806327e235e31461006c57806370a082311461009c578063a9059cbb146100cc575b5f5ffd5b6100566100fc565b60405161006391906102a1565b60405180910390f35b61008660048036038101906100819190610318565b610102565b60405161009391906102a1565b60405180910390f35b6100b660048036038101906100b19190610318565b610116565b6040516100c391906102a1565b60405180910390f35b6100e660048036038101906100e1919061036d565b61015b565b6040516100f391906103c5565b60405180910390f35b60015481565b5f602052805f5260405f205f915090505481565b5f5f5f8373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f20549050919050565b5f815f5f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205410156101db576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016101d290610438565b60405180910390fd5b815f5f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546102269190610483565b92505081905550815f5f8573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f82825461027891906104b6565b925050819055506001905092915050565b5f819050919050565b61029b81610289565b82525050565b5f6020820190506102b45f830184610292565b92915050565b5f5ffd5b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f6102e7826102be565b9050919050565b6102f7816102dd565b8114610301575f5ffd5b50565b5f81359050610312816102ee565b92915050565b5f6020828403121561032d5761032c6102ba565b5b5f61033a84828501610304565b91505092915050565b61034c81610289565b8114610356575f5ffd5b50565b5f8135905061036781610343565b92915050565b5f5f60408385031215610383576103826102ba565b5b5f61039085828601610304565b92505060206103a185828601610359565b9150509250929050565b5f8115159050919050565b6103bf816103ab565b82525050565b5f6020820190506103d85f8301846103b6565b92915050565b5f82825260208201905092915050565b7f496e73756666696369656e742062616c616e63650000000000000000000000005f82015250565b5f6104226014836103de565b915061042d826103ee565b602082019050919050565b5f6020820190508181035f83015261044f81610416565b9050919050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f61048d82610289565b915061049883610289565b92508282039050818111156104b0576104af610456565b5b92915050565b5f6104c082610289565b91506104cb83610289565b92508282019050808211156104e3576104e2610456565b5b9291505056fea2646970667358221220451e82a180ed304d8b01f029cf6b2976b0cc2d03eb535a89648a4ef6a6d6a73664736f6c634300081e0033c001a0416acd3bfefd89cbe25745ef8752be3de00f4db63be0a3429300d2c75812fdafa01609998d8d0be85169a023581dc0bcc7352e83d4ead44b669de6c944409f8b82");
const DEPLOYMENT_TX_HASH: B256 =
    b256!("0xdad2b96d4c535d23274b675e05238197b92e34d2d7545a8401a5faaba36d8318");

// 2. transfer: Alice transfers 100 tokens to Bob (nonce 1)
const TRANSFER_TO_BOB_TX: Bytes = bytes!("0x02f8ae83014a340101847735940182c4a4945fbdb2315678afecb367f032d93f642f64180aa380b844a9059cbb00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c80000000000000000000000000000000000000000000000000000000000000064c001a09c79ed17167e13a7850dd0c22a02a2327e6bc13e923b0e40c79ddfa089f90311a03246cebb13a7c70b31c2af87d3d85ad6b40fc5404c127bb7e5efd545ba63fc68");
const TRANSFER_TO_BOB_TX_HASH: B256 =
    b256!("0xaad9de702ede9dfff3c97331b42baf3c98b2a6ca5411547072b9c7befefd563e");

// 3. transfer: Alice transfers 50 tokens to Charlie (nonce 2)
const TRANSFER_TO_CHARLIE_TX: Bytes = bytes!("0x02f8ae83014a3402018468afe50d82c498945fbdb2315678afecb367f032d93f642f64180aa380b844a9059cbb0000000000000000000000003c44cdddb6a900fa2b585dd299e03d12fa4293bc0000000000000000000000000000000000000000000000000000000000000032c080a0e81e5ff1e174b27905fabb9f02ba425f5ca0b2af6b50c2f9e32fdc4fa7df4e46a04018b7cd35a7cc9823c0c63c6e785e99fb1e38925c25b49417246729b9c7c387");
const TRANSFER_TO_CHARLIE_TX_HASH: B256 =
    b256!("0xac46da5c2d18b51782a857ef594efe854109c022f2ccfc47e6f648b8a87c3343");

// balanceOf(address): 0x70a08231
// transfer(address,uint256): 0xa9059cbb
// totalSupply(): 0x18160ddd

fn encode_balance_of(account: Address) -> TransactionInput {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]); // balanceOf selector
    data.extend_from_slice(&[0; 12]); // padding
    data.extend_from_slice(account.as_slice());
    TransactionInput::new(Bytes::from(data))
}

fn parse_uint256(result: Bytes) -> U256 {
    if result.len() >= 32 {
        U256::from_be_slice(&result[result.len() - 32..])
    } else {
        U256::ZERO
    }
}

fn create_erc20_flashblock_delta(
    flashblock_index: u64,
    tx_hash: B256,
    tx: Bytes,
    cumulative_gas_used: u64,
) -> Flashblock {
    let mut receipts = HashMap::default();

    receipts.insert(
        tx_hash,
        OpReceipt::Eip1559(Receipt {
            status: true.into(),
            cumulative_gas_used, // all txs in block
            logs: vec![],
        }),
    );

    Flashblock {
        payload_id: PayloadId::new([0; 8]),
        index: flashblock_index,
        base: None, // no base
        diff: ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![tx],
            ..Default::default()
        },
        metadata: Metadata {
            block_number: 1,
            receipts,
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn test_erc20_multi_user_balances() -> eyre::Result<()> {
    let node = setup_node().await?;
    let provider = node.provider().await?;

    let mut base_flashblock = create_base_flashblock(1, vec![]);
    base_flashblock.index = 0;
    node.send_payload(base_flashblock).await?;

    // gas = BLOCK_INFO (21000) + DEPLOYMENT (380000) = 401000
    let flashblock1 = create_erc20_flashblock_delta(1, DEPLOYMENT_TX_HASH, DEPLOYMENT_TX, 401000);
    node.send_payload(flashblock1).await?;

    let alice_balance = get_balance(&provider, ALICE).await?;
    assert_eq!(
        alice_balance,
        U256::from(1000),
        "Alice should have 1000 tokens after deployment"
    );

    let bob_balance = get_balance(&provider, BOB).await?;
    assert_eq!(
        bob_balance,
        U256::ZERO,
        "Bob should have 0 tokens initially"
    );
    let charlie_balance = get_balance(&provider, CHARLIE).await?;
    assert_eq!(
        charlie_balance,
        U256::ZERO,
        "Charlie should have 0 tokens initially"
    );

    // gas = BLOCK_INFO (21000) + DEPLOYMENT (380000) + TRANSFER_BOB (50000) = 451000
    let flashblock2 =
        create_erc20_flashblock_delta(2, TRANSFER_TO_BOB_TX_HASH, TRANSFER_TO_BOB_TX, 451000);
    node.send_payload(flashblock2).await?;

    let alice_balance = get_balance(&provider, ALICE).await?;
    assert_eq!(
        alice_balance,
        U256::from(900),
        "Alice should have 900 tokens after transfer to Bob"
    );

    let bob_balance = get_balance(&provider, BOB).await?;
    assert_eq!(bob_balance, U256::from(100), "Bob should have 100 tokens");

    // gas = BLOCK_INFO (21000) + DEPLOYMENT (380000) + TRANSFER_BOB (50000) + TRANSFER_CHARLIE (50000) = 501000
    let flashblock3 = create_erc20_flashblock_delta(
        3,
        TRANSFER_TO_CHARLIE_TX_HASH,
        TRANSFER_TO_CHARLIE_TX,
        501000,
    );
    node.send_payload(flashblock3).await?;

    let alice_balance = get_balance(&provider, ALICE).await?;
    assert_eq!(
        alice_balance,
        U256::from(850),
        "Alice should have 850 tokens after both transfers"
    );

    let bob_balance = get_balance(&provider, BOB).await?;
    assert_eq!(
        bob_balance,
        U256::from(100),
        "Bob should still have 100 tokens"
    );

    let charlie_balance = get_balance(&provider, CHARLIE).await?;
    assert_eq!(
        charlie_balance,
        U256::from(50),
        "Charlie should have 50 tokens"
    );

    Ok(())
}

async fn get_balance(provider: &RootProvider<Optimism>, account: Address) -> eyre::Result<U256> {
    let call = OpTransactionRequest::default()
        .from(ALICE)
        .to(ERC20_ADDRESS)
        .gas_limit(100_000)
        .input(encode_balance_of(account));

    let result = provider.call(call.into()).await?;
    Ok(parse_uint256(result))
}

#[tokio::test]
async fn test_basic_eth_call_with_flashblocks() -> eyre::Result<()> {
    let node = setup_node().await?;
    let provider = node.provider().await?;

    let flashblock = create_base_flashblock(1, vec![]);
    node.send_payload(flashblock).await?;

    let call = OpTransactionRequest::default()
        .from(ALICE)
        .to(BOB)
        .gas_limit(21_000);

    let result = provider.call(call).await;
    assert!(result.is_ok(), "eth_call should work with flashblocks");

    Ok(())
}

#[tokio::test]
async fn test_erc20_balance_progression() -> eyre::Result<()> {
    let node = setup_node().await?;
    let provider = node.provider().await?;

    let mut base_flashblock = create_base_flashblock(1, vec![]);
    base_flashblock.index = 0;
    node.send_payload(base_flashblock).await?;

    // gas = BLOCK_INFO (21000) + DEPLOYMENT (380000) = 401000
    let flashblock1 = create_erc20_flashblock_delta(1, DEPLOYMENT_TX_HASH, DEPLOYMENT_TX, 401000);
    node.send_payload(flashblock1).await?;

    let balance = get_balance(&provider, ALICE).await?;
    assert_eq!(
        balance,
        U256::from(1000),
        "After deployment: Alice should have 1000"
    );

    // gas = BLOCK_INFO (21000) + DEPLOYMENT (380000) + TRANSFER_BOB (50000) = 451000
    let flashblock2 =
        create_erc20_flashblock_delta(2, TRANSFER_TO_BOB_TX_HASH, TRANSFER_TO_BOB_TX, 451000);
    node.send_payload(flashblock2).await?;

    let balance = get_balance(&provider, ALICE).await?;
    assert_eq!(
        balance,
        U256::from(900),
        "After transfer to Bob: Alice should have 900"
    );

    // gas = BLOCK_INFO (21000) + DEPLOYMENT (380000) + TRANSFER_BOB (50000) + TRANSFER_CHARLIE (50000) = 501000
    let flashblock3 = create_erc20_flashblock_delta(
        3,
        TRANSFER_TO_CHARLIE_TX_HASH,
        TRANSFER_TO_CHARLIE_TX,
        501000,
    );
    node.send_payload(flashblock3).await?;

    let balance = get_balance(&provider, ALICE).await?;
    assert_eq!(
        balance,
        U256::from(850),
        "After transfer to Charlie: Alice should have 850"
    );

    Ok(())
}
