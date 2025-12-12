//! Integration tests for `eth_call` with ERC-20 token operations.
//!
//! Tests cover:
//! - Basic ERC-20 transfer
//! - Mint new tokens
//! - Burn existing tokens
//! - Approve + transferFrom flow
//! - TransparentProxy with ERC-20 (USDC-style):
//!   - Transfer through proxy
//!   - Mint through proxy
//!   - Burn through proxy
//!   - Approve + transferFrom through proxy

use std::str::FromStr;

use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_sol_macro::sol;
use alloy_sol_types::SolConstructor;
use base_reth_test_utils::harness::TestHarness;
use eyre::Result;

sol!(
    #[sol(rpc)]
    TestERC20,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/TestERC20.sol/TestERC20.json"
    )
);

sol!(
    #[sol(rpc)]
    TransparentProxy,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/TransparentProxy.sol/TransparentProxy.json"
    )
);

/// Test setup containing harness, token contract address, and proxy address
struct Erc20TestSetup {
    harness: TestHarness,
    token_address: Address,
    proxy_address: Option<Address>,
    deployer_nonce: u64,
}

impl Erc20TestSetup {
    /// Deploy ERC-20 token and optionally a TransparentProxy
    async fn new(with_proxy: bool) -> Result<Self> {
        let harness = TestHarness::new().await?;
        let deployer = &harness.accounts().deployer;

        // Deploy TestERC20 contract
        // Constructor args: name, symbol, decimals
        let constructor_args = TestERC20::constructorCall {
            _name: "Test Token".to_string(),
            _symbol: "TEST".to_string(),
            _decimals: 18,
        };
        let deploy_data = [TestERC20::BYTECODE.to_vec(), constructor_args.abi_encode()].concat();

        let (token_deploy_tx, token_address, _) =
            deployer.create_deployment_tx(Bytes::from(deploy_data), 0)?;

        // Build block with token deployment
        harness.build_block_from_transactions(vec![token_deploy_tx]).await?;

        let (proxy_address, deployer_nonce) = if with_proxy {
            // Deploy TransparentProxy pointing to token implementation
            let proxy_constructor =
                TransparentProxy::constructorCall { _implementation: token_address };
            let proxy_deploy_data =
                [TransparentProxy::BYTECODE.to_vec(), proxy_constructor.abi_encode()].concat();

            let (proxy_deploy_tx, proxy_addr, _) =
                deployer.create_deployment_tx(Bytes::from(proxy_deploy_data), 1)?;

            harness.build_block_from_transactions(vec![proxy_deploy_tx]).await?;
            (Some(proxy_addr), 2)
        } else {
            (None, 1)
        };

        Ok(Self { harness, token_address, proxy_address, deployer_nonce })
    }

    /// Get the address to interact with (proxy if available, otherwise token directly)
    fn interaction_address(&self) -> Address {
        self.proxy_address.unwrap_or(self.token_address)
    }
}

#[tokio::test]
async fn test_erc20_transfer() -> Result<()> {
    let setup = Erc20TestSetup::new(false).await?;
    let provider = setup.harness.provider();
    let accounts = setup.harness.accounts();
    let token_address = setup.token_address;

    let token = TestERC20::TestERC20Instance::new(token_address, provider.clone());

    // First mint some tokens to Alice
    let mint_tx =
        token.mint(accounts.alice.address, U256::from(1000u64)).into_transaction_request();
    let (mint_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_tx.nonce(setup.deployer_nonce))?;
    setup.harness.build_block_from_transactions(vec![mint_tx_bytes]).await?;

    // Verify Alice's balance via eth_call
    let balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let balance_result = provider.call(balance_call).await?;
    let alice_balance = U256::from_str(&balance_result.to_string())?;
    assert_eq!(alice_balance, U256::from(1000u64));

    // Transfer from Alice to Bob via actual transaction
    let transfer_tx = token
        .transfer(accounts.bob.address, U256::from(300u64))
        .into_transaction_request()
        .from(accounts.alice.address);
    let (transfer_tx_bytes, _) = accounts.alice.sign_txn_request(transfer_tx.nonce(0))?;
    setup.harness.build_block_from_transactions(vec![transfer_tx_bytes]).await?;

    // Verify balances after transfer
    let alice_balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let alice_balance_result = provider.call(alice_balance_call).await?;
    let alice_balance_after = U256::from_str(&alice_balance_result.to_string())?;
    assert_eq!(alice_balance_after, U256::from(700u64));

    let bob_balance_call = token.balanceOf(accounts.bob.address).into_transaction_request();
    let bob_balance_result = provider.call(bob_balance_call).await?;
    let bob_balance = U256::from_str(&bob_balance_result.to_string())?;
    assert_eq!(bob_balance, U256::from(300u64));

    Ok(())
}

#[tokio::test]
async fn test_erc20_mint() -> Result<()> {
    let setup = Erc20TestSetup::new(false).await?;
    let provider = setup.harness.provider();
    let accounts = setup.harness.accounts();
    let token_address = setup.token_address;

    let token = TestERC20::TestERC20Instance::new(token_address, provider.clone());

    // Check initial balance is zero
    let initial_balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let initial_result = provider.call(initial_balance_call).await?;
    let initial_balance = U256::from_str(&initial_result.to_string())?;
    assert_eq!(initial_balance, U256::ZERO);

    // Check initial total supply
    let initial_supply_call = token.totalSupply().into_transaction_request();
    let initial_supply_result = provider.call(initial_supply_call).await?;
    let initial_supply = U256::from_str(&initial_supply_result.to_string())?;
    assert_eq!(initial_supply, U256::ZERO);

    // Mint tokens to Alice
    let mint_amount = U256::from(5000u64);
    let mint_tx = token.mint(accounts.alice.address, mint_amount).into_transaction_request();
    let (mint_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_tx.nonce(setup.deployer_nonce))?;
    setup.harness.build_block_from_transactions(vec![mint_tx_bytes]).await?;

    // Verify Alice's balance increased
    let balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let balance_result = provider.call(balance_call).await?;
    let balance = U256::from_str(&balance_result.to_string())?;
    assert_eq!(balance, mint_amount);

    // Verify total supply increased
    let supply_call = token.totalSupply().into_transaction_request();
    let supply_result = provider.call(supply_call).await?;
    let supply = U256::from_str(&supply_result.to_string())?;
    assert_eq!(supply, mint_amount);

    // Mint more tokens to Bob
    let mint_bob_amount = U256::from(3000u64);
    let mint_bob_tx = token.mint(accounts.bob.address, mint_bob_amount).into_transaction_request();
    let (mint_bob_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_bob_tx.nonce(setup.deployer_nonce + 1))?;
    setup.harness.build_block_from_transactions(vec![mint_bob_tx_bytes]).await?;

    // Verify Bob's balance
    let bob_balance_call = token.balanceOf(accounts.bob.address).into_transaction_request();
    let bob_balance_result = provider.call(bob_balance_call).await?;
    let bob_balance = U256::from_str(&bob_balance_result.to_string())?;
    assert_eq!(bob_balance, mint_bob_amount);

    // Verify total supply is sum of both mints
    let final_supply_call = token.totalSupply().into_transaction_request();
    let final_supply_result = provider.call(final_supply_call).await?;
    let final_supply = U256::from_str(&final_supply_result.to_string())?;
    assert_eq!(final_supply, mint_amount + mint_bob_amount);

    Ok(())
}

#[tokio::test]
async fn test_erc20_burn() -> Result<()> {
    let setup = Erc20TestSetup::new(false).await?;
    let provider = setup.harness.provider();
    let accounts = setup.harness.accounts();
    let token_address = setup.token_address;

    let token = TestERC20::TestERC20Instance::new(token_address, provider.clone());

    // Mint tokens to Alice first
    let mint_amount = U256::from(1000u64);
    let mint_tx = token.mint(accounts.alice.address, mint_amount).into_transaction_request();
    let (mint_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_tx.nonce(setup.deployer_nonce))?;
    setup.harness.build_block_from_transactions(vec![mint_tx_bytes]).await?;

    // Verify initial balance
    let balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let balance_result = provider.call(balance_call).await?;
    let balance = U256::from_str(&balance_result.to_string())?;
    assert_eq!(balance, mint_amount);

    // Burn some tokens from Alice
    let burn_amount = U256::from(400u64);
    let burn_tx = token.burn(accounts.alice.address, burn_amount).into_transaction_request();
    let (burn_tx_bytes, _) =
        accounts.deployer.sign_txn_request(burn_tx.nonce(setup.deployer_nonce + 1))?;
    setup.harness.build_block_from_transactions(vec![burn_tx_bytes]).await?;

    // Verify Alice's balance decreased
    let balance_after_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let balance_after_result = provider.call(balance_after_call).await?;
    let balance_after = U256::from_str(&balance_after_result.to_string())?;
    assert_eq!(balance_after, mint_amount - burn_amount);

    // Verify total supply decreased
    let supply_call = token.totalSupply().into_transaction_request();
    let supply_result = provider.call(supply_call).await?;
    let supply = U256::from_str(&supply_result.to_string())?;
    assert_eq!(supply, mint_amount - burn_amount);

    Ok(())
}

#[tokio::test]
async fn test_erc20_approve_transfer_from() -> Result<()> {
    let setup = Erc20TestSetup::new(false).await?;
    let provider = setup.harness.provider();
    let accounts = setup.harness.accounts();
    let token_address = setup.token_address;

    let token = TestERC20::TestERC20Instance::new(token_address, provider.clone());

    // Mint tokens to Alice
    let mint_amount = U256::from(1000u64);
    let mint_tx = token.mint(accounts.alice.address, mint_amount).into_transaction_request();
    let (mint_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_tx.nonce(setup.deployer_nonce))?;
    setup.harness.build_block_from_transactions(vec![mint_tx_bytes]).await?;

    // Alice approves Bob to spend 500 tokens
    let approve_amount = U256::from(500u64);
    let approve_tx = token
        .approve(accounts.bob.address, approve_amount)
        .into_transaction_request()
        .from(accounts.alice.address);
    let (approve_tx_bytes, _) = accounts.alice.sign_txn_request(approve_tx.nonce(0))?;
    setup.harness.build_block_from_transactions(vec![approve_tx_bytes]).await?;

    // Verify allowance via eth_call
    let allowance_call =
        token.allowance(accounts.alice.address, accounts.bob.address).into_transaction_request();
    let allowance_result = provider.call(allowance_call).await?;
    let allowance = U256::from_str(&allowance_result.to_string())?;
    assert_eq!(allowance, approve_amount);

    // Bob transfers 300 tokens from Alice to Charlie using transferFrom
    let transfer_amount = U256::from(300u64);
    let transfer_from_tx = token
        .transferFrom(accounts.alice.address, accounts.charlie.address, transfer_amount)
        .into_transaction_request()
        .from(accounts.bob.address);
    let (transfer_from_tx_bytes, _) = accounts.bob.sign_txn_request(transfer_from_tx.nonce(0))?;
    setup.harness.build_block_from_transactions(vec![transfer_from_tx_bytes]).await?;

    // Verify Charlie received the tokens
    let charlie_balance_call = token.balanceOf(accounts.charlie.address).into_transaction_request();
    let charlie_balance_result = provider.call(charlie_balance_call).await?;
    let charlie_balance = U256::from_str(&charlie_balance_result.to_string())?;
    assert_eq!(charlie_balance, transfer_amount);

    // Verify Alice's balance decreased
    let alice_balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let alice_balance_result = provider.call(alice_balance_call).await?;
    let alice_balance = U256::from_str(&alice_balance_result.to_string())?;
    assert_eq!(alice_balance, mint_amount - transfer_amount);

    // Verify remaining allowance
    let remaining_allowance_call =
        token.allowance(accounts.alice.address, accounts.bob.address).into_transaction_request();
    let remaining_allowance_result = provider.call(remaining_allowance_call).await?;
    let remaining_allowance = U256::from_str(&remaining_allowance_result.to_string())?;
    assert_eq!(remaining_allowance, approve_amount - transfer_amount);

    Ok(())
}

#[tokio::test]
async fn test_transparent_proxy_erc20_transfer() -> Result<()> {
    let setup = Erc20TestSetup::new(true).await?;
    let provider = setup.harness.provider();
    let accounts = setup.harness.accounts();
    let proxy_address = setup.interaction_address();

    // Interact with token through proxy
    let token = TestERC20::TestERC20Instance::new(proxy_address, provider.clone());

    // Mint tokens through proxy
    let mint_tx =
        token.mint(accounts.alice.address, U256::from(1000u64)).into_transaction_request();
    let (mint_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_tx.nonce(setup.deployer_nonce))?;
    setup.harness.build_block_from_transactions(vec![mint_tx_bytes]).await?;

    // Verify balance through proxy
    let balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let balance_result = provider.call(balance_call).await?;
    let balance = U256::from_str(&balance_result.to_string())?;
    assert_eq!(balance, U256::from(1000u64));

    // Transfer through proxy
    let transfer_tx = token
        .transfer(accounts.bob.address, U256::from(400u64))
        .into_transaction_request()
        .from(accounts.alice.address);
    let (transfer_tx_bytes, _) = accounts.alice.sign_txn_request(transfer_tx.nonce(0))?;
    setup.harness.build_block_from_transactions(vec![transfer_tx_bytes]).await?;

    // Verify balances after transfer
    let alice_balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let alice_balance_result = provider.call(alice_balance_call).await?;
    let alice_balance = U256::from_str(&alice_balance_result.to_string())?;
    assert_eq!(alice_balance, U256::from(600u64));

    let bob_balance_call = token.balanceOf(accounts.bob.address).into_transaction_request();
    let bob_balance_result = provider.call(bob_balance_call).await?;
    let bob_balance = U256::from_str(&bob_balance_result.to_string())?;
    assert_eq!(bob_balance, U256::from(400u64));

    Ok(())
}

#[tokio::test]
async fn test_transparent_proxy_erc20_approve_transfer_from() -> Result<()> {
    let setup = Erc20TestSetup::new(true).await?;
    let provider = setup.harness.provider();
    let accounts = setup.harness.accounts();
    let proxy_address = setup.interaction_address();

    // Interact with token through proxy
    let token = TestERC20::TestERC20Instance::new(proxy_address, provider.clone());

    // Mint tokens to Alice through proxy
    let mint_tx =
        token.mint(accounts.alice.address, U256::from(2000u64)).into_transaction_request();
    let (mint_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_tx.nonce(setup.deployer_nonce))?;
    setup.harness.build_block_from_transactions(vec![mint_tx_bytes]).await?;

    // Alice approves Bob through proxy
    let approve_tx = token
        .approve(accounts.bob.address, U256::from(800u64))
        .into_transaction_request()
        .from(accounts.alice.address);
    let (approve_tx_bytes, _) = accounts.alice.sign_txn_request(approve_tx.nonce(0))?;
    setup.harness.build_block_from_transactions(vec![approve_tx_bytes]).await?;

    // Verify allowance through proxy
    let allowance_call =
        token.allowance(accounts.alice.address, accounts.bob.address).into_transaction_request();
    let allowance_result = provider.call(allowance_call).await?;
    let allowance = U256::from_str(&allowance_result.to_string())?;
    assert_eq!(allowance, U256::from(800u64));

    // Bob transfers from Alice to Charlie through proxy
    let transfer_from_tx = token
        .transferFrom(accounts.alice.address, accounts.charlie.address, U256::from(500u64))
        .into_transaction_request()
        .from(accounts.bob.address);
    let (transfer_from_tx_bytes, _) = accounts.bob.sign_txn_request(transfer_from_tx.nonce(0))?;
    setup.harness.build_block_from_transactions(vec![transfer_from_tx_bytes]).await?;

    // Verify Charlie's balance through proxy
    let charlie_balance_call = token.balanceOf(accounts.charlie.address).into_transaction_request();
    let charlie_balance_result = provider.call(charlie_balance_call).await?;
    let charlie_balance = U256::from_str(&charlie_balance_result.to_string())?;
    assert_eq!(charlie_balance, U256::from(500u64));

    // Verify remaining allowance
    let remaining_allowance_call =
        token.allowance(accounts.alice.address, accounts.bob.address).into_transaction_request();
    let remaining_allowance_result = provider.call(remaining_allowance_call).await?;
    let remaining_allowance = U256::from_str(&remaining_allowance_result.to_string())?;
    assert_eq!(remaining_allowance, U256::from(300u64));

    Ok(())
}

#[tokio::test]
async fn test_transparent_proxy_erc20_mint() -> Result<()> {
    let setup = Erc20TestSetup::new(true).await?;
    let provider = setup.harness.provider();
    let accounts = setup.harness.accounts();
    let proxy_address = setup.interaction_address();

    // Interact with token through proxy
    let token = TestERC20::TestERC20Instance::new(proxy_address, provider.clone());

    // Check initial balance is zero through proxy
    let initial_balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let initial_result = provider.call(initial_balance_call).await?;
    let initial_balance = U256::from_str(&initial_result.to_string())?;
    assert_eq!(initial_balance, U256::ZERO);

    // Check initial total supply through proxy
    let initial_supply_call = token.totalSupply().into_transaction_request();
    let initial_supply_result = provider.call(initial_supply_call).await?;
    let initial_supply = U256::from_str(&initial_supply_result.to_string())?;
    assert_eq!(initial_supply, U256::ZERO);

    // Mint tokens to Alice through proxy
    let mint_amount = U256::from(7500u64);
    let mint_tx = token.mint(accounts.alice.address, mint_amount).into_transaction_request();
    let (mint_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_tx.nonce(setup.deployer_nonce))?;
    setup.harness.build_block_from_transactions(vec![mint_tx_bytes]).await?;

    // Verify Alice's balance increased through proxy
    let balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let balance_result = provider.call(balance_call).await?;
    let balance = U256::from_str(&balance_result.to_string())?;
    assert_eq!(balance, mint_amount);

    // Verify total supply increased through proxy
    let supply_call = token.totalSupply().into_transaction_request();
    let supply_result = provider.call(supply_call).await?;
    let supply = U256::from_str(&supply_result.to_string())?;
    assert_eq!(supply, mint_amount);

    // Mint more tokens to Bob through proxy
    let mint_bob_amount = U256::from(2500u64);
    let mint_bob_tx = token.mint(accounts.bob.address, mint_bob_amount).into_transaction_request();
    let (mint_bob_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_bob_tx.nonce(setup.deployer_nonce + 1))?;
    setup.harness.build_block_from_transactions(vec![mint_bob_tx_bytes]).await?;

    // Verify Bob's balance through proxy
    let bob_balance_call = token.balanceOf(accounts.bob.address).into_transaction_request();
    let bob_balance_result = provider.call(bob_balance_call).await?;
    let bob_balance = U256::from_str(&bob_balance_result.to_string())?;
    assert_eq!(bob_balance, mint_bob_amount);

    // Verify total supply is sum of both mints through proxy
    let final_supply_call = token.totalSupply().into_transaction_request();
    let final_supply_result = provider.call(final_supply_call).await?;
    let final_supply = U256::from_str(&final_supply_result.to_string())?;
    assert_eq!(final_supply, mint_amount + mint_bob_amount);

    Ok(())
}

#[tokio::test]
async fn test_transparent_proxy_erc20_burn() -> Result<()> {
    let setup = Erc20TestSetup::new(true).await?;
    let provider = setup.harness.provider();
    let accounts = setup.harness.accounts();
    let proxy_address = setup.interaction_address();

    // Interact with token through proxy
    let token = TestERC20::TestERC20Instance::new(proxy_address, provider.clone());

    // Mint tokens to Alice first through proxy
    let mint_amount = U256::from(1500u64);
    let mint_tx = token.mint(accounts.alice.address, mint_amount).into_transaction_request();
    let (mint_tx_bytes, _) =
        accounts.deployer.sign_txn_request(mint_tx.nonce(setup.deployer_nonce))?;
    setup.harness.build_block_from_transactions(vec![mint_tx_bytes]).await?;

    // Verify initial balance through proxy
    let balance_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let balance_result = provider.call(balance_call).await?;
    let balance = U256::from_str(&balance_result.to_string())?;
    assert_eq!(balance, mint_amount);

    // Verify initial total supply through proxy
    let initial_supply_call = token.totalSupply().into_transaction_request();
    let initial_supply_result = provider.call(initial_supply_call).await?;
    let initial_supply = U256::from_str(&initial_supply_result.to_string())?;
    assert_eq!(initial_supply, mint_amount);

    // Burn some tokens from Alice through proxy
    let burn_amount = U256::from(600u64);
    let burn_tx = token.burn(accounts.alice.address, burn_amount).into_transaction_request();
    let (burn_tx_bytes, _) =
        accounts.deployer.sign_txn_request(burn_tx.nonce(setup.deployer_nonce + 1))?;
    setup.harness.build_block_from_transactions(vec![burn_tx_bytes]).await?;

    // Verify Alice's balance decreased through proxy
    let balance_after_call = token.balanceOf(accounts.alice.address).into_transaction_request();
    let balance_after_result = provider.call(balance_after_call).await?;
    let balance_after = U256::from_str(&balance_after_result.to_string())?;
    assert_eq!(balance_after, mint_amount - burn_amount);

    // Verify total supply decreased through proxy
    let supply_call = token.totalSupply().into_transaction_request();
    let supply_result = provider.call(supply_call).await?;
    let supply = U256::from_str(&supply_result.to_string())?;
    assert_eq!(supply, mint_amount - burn_amount);

    Ok(())
}
