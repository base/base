# Token Bridging

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->

- [Overview](#overview)
- [Implementation](#implementation)
- [Invariants](#invariants)
- [Deployment and migrations](#deployment-and-migrations)
  - [Factory](#factory)
  - [Migration to the Standard](#migration-to-the-standard)
- [Future Considerations](#future-considerations)
  - [Cross Chain `transferFrom`](#cross-chain-transferfrom)
    - [Approvals](#approvals)
    - [`remoteTransferFrom()`](#remotetransferfrom)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Without a standardized security model, bridged assets may not be fungible with each other. The `SuperchainERC20` unifies ERC20 token bridging to make it fungible across the Superchain. It builds on top of the messaging protocol, as the most trust minimized bridging solution.

Unlike other standards, such as [xERC20](https://www.xerc20.com/), where users interact with a bridge possessing mint and burn privileges on cross-chain enabled tokens, this approach allows users to call methods directly on the token contract.

## Implementation

The standard will build on top of ERC20 and include the following five external functions:

- `sendERC20(uint256 amount, uint256 chainId)(bool)`
- `sendERC20(address to, uint256 amount, uint256 chainId)(bool)`
- `sendERC20(address to, uint256 amount, uint256 chainId, bytes data)(bool)`
- `finalizeSendERC20(address to, uint256 amount)`
- `finalizeSendERC20(address to, uint256 amount, bytes data)`

`sendERC20(uint256 amount, uint256 chainId)` and `sendERC20To(address to, uint256 amount, uint256 chainId)` will burn `amount` tokens and initialize a message to the `L2ToL2CrossChainMessenger` to mint the `amount` in the target address at `chainId`. `sendERC20(address to, uint256 amount, uint256 chainId, bytes data)` will do the same, but also include a message.

`finalizeSendERC20(address to, uint256 amount)` will process incoming messages from the `L2ToL2CrossChainMessenger` initiated from the same token on a different `chainId` and mint `amount` to the `to` address. `finalizeSendERC20(address to, uint256 amount, bytes data)` will do the same, but include an external call to address `to` with `data`.

An example implementation that depends on deterministic deployments across chains
for security is provided. This construction builds on top of the [L2ToL2CrossDomainMessenger][l2-to-l2]
for both replay protection and domain binding.

[l2-to-l2]: ./predeploys.md#l2tol2crossdomainmessenger

[TODO] should we add a sendERC20To specific event?

```solidity
function sendERC20(uint256 _amount, uint256 _chainId) external returns (bool) {
  return sendERC20(msg.sender, _amount, _chainId);
}

function sendERC20(address _to, uint256 _amount, uint256 _chainId) public returns (bool success) {
  _burn(msg.sender, _amount);
  bytes memory _message = abi.encodeWithSignature("finalizeSendERC20(address,uint256)", _to, _amount);
  _sendMessage(_chainId, _message);
  return true
}

function sendERC20(address _to, uint256 _amount, uint256 _chainId, bytes memory _data) public returns (bool success) {
  _burn(msg.sender, _amount);
  bytes memory _message = abi.encodeWithSignature("finalizeSendERC20(address,uint256,bytes)", _to, _amount, _data);
  _sendMessage(_chainId, _message);
  return true
}

function finalizeSendERC20(address _to, uint256 _amount) external {
  // this will be a modifier, inline here for clarity
  require(msg.sender == address(L2ToL2CrossChainMessenger));
  require(L2ToL2CrossChainMessenger.crossDomainMessageSender() == address(this));
  //
  _mint(_to, _amount);
}

function finalizeSendERC20(address _to, uint256 _amount, bytes memory _data) external {
  require(msg.sender == address(L2ToL2CrossChainMessenger));
  require(L2ToL2CrossChainMessenger.crossDomainMessageSender() == address(this));
  _mint(_to, _amount);
  (bool success, ) = _to.call(_data);
  require(success, "External call failed");
}

/////// Internal functions ///////////
function _sendMessage(uint256 _destination, bytes memory _data) internal virtual {
  L2ToL2CrossDomainMessenger.sendMessage(_destination, address(this), _data);
}
```

Note: Some other naming options that were considered were `bridgeERC20`, `send()`, `xTransfer()`, `Transfer` (with a chainId parameter).

## Invariants

Besides the ERC20 invariants, the SuperchainERC20 will require the following interop specific properties:

- Conservation of finalized `totalSupply`: The minted `amount` in `finalizeSendERC20()` should match the `amount` that was burnt in `sendERC20()`, as long as target chain has the initiating chain in the dependency set.
  - Corollary 1: Cross-chain transactions will conserve the sum of `totalSupply` for each chain in the Superchain across safe blocks.
  - Corollary 2: Each initiated but not finalized message (included in initiating chain but not yet in target chain) will decrease the `totalSupply` exactly by the burnt `amount`.
  - Corollary 2: `SuperchainERC20s` should not charge a token fee or increase the balance when moving cross-chain.
  - Question: what should we assume for chains not in the dependency set? a rollback method could modify this invariant to also consider that case.
- Freedom of movement: Users should be able to send tokens into any target chain that has initiating chain in its dependency set.
  - Question: should we add "deployed" to the invariant or assume its not going to be an issue?
- [To discuss] Unique Messenger: The `sendERC20()` functions must exclusively use the `L2toL2CrossDomainMessenger` for messaging. Similarly, the `finalizeSendERC20()` function should only process messages originating from the L2toL2CrossDomainMessenger.
  - Corollary: xERC20 and other standards from third party bridges should use different functions.
- [To discuss] Locally initiated: The bridging action should be initialized from the chain where funds are located only.
  - This is because same address might correspond to different users cross-chain. For example, two SAFEs with the same address in two chains might have different owners. It it possible to distiguish an EOA from a contract by checking the size of the code in the caller's address, but this will probably change with [EIP-7702](https://github.com/ethereum/EIPs/blob/master/EIPS/eip-7702.md).
  - A way to allow for remotely initiated bridging is to include remote approval, i.e. approve a certain address in a certain chainId to spend local funds.
- [To discuss] Bridge Event: `sendERC20()` should emit a `Bridged` event.
  - We already have a lot of events triggering from these actions. Should we include yet another one?

**Desired properties**

- Spatial symmetry: Same implementation and address across chains.

**Open questions**

- Should we define invariants for upgrades?
- Does native minting introduce any invariant?
  - If every factory is "equal", how do you choose which one creates the initial supply?

## Deployment and migrations

### Factory

A token factory predeploy can ensure that `SuperchainERC20` tokens can be permissionlessly and deterministically deployed. Without a deterministic deployment scheme, maintaining a mapping of the token addresses between all of the chains will not be scalable.

[TODO] We will create a design doc that covers `SuperchainERC20` factories and link it in this specs.

### Migration to the Standard

- New tokens that want to be interoperable should implement the `SuperchainERC20` standard from inception.
- Tokens that are already deployed and upgradable, can update the implementation to be `SuperchainERC20` compatible.
- Tokens that are already deployed but are not upgradable should use a different method.
  - Some options are wrapping, converting (if burn-mint rights can be modified) or Mirroring.
- Tokens that are `OptimismMintableERC20Token` (corresponding to locked liquidity in L1) fall into the above category of already deployed and non updatable. They do have a special property though, which is burn/mint permissions granted to the `L2StandardBridge`. This makes the convert method particularly appealing. See [Liquidity Migration](https://github.com/ethereum-optimism/design-docs/blob/098435155471ed3bcd60a50f049897495c901733/protocol/superc20/liquidity-migration.md) for more context.

## Future Considerations

### Cross Chain `transferFrom`

In addition to standard locally initialized bridging, it is possible to allow contracts to be cross-chain interoperable. For example, a contract in chain A could send pre-approved funds from a user in chain B to a contract in chain C.

The standard should not include a remotely initialized `transfer` implementation as this should be handled directly on the chain where the funds live with the `sendERC20()` function. `transferFrom`, on the other hand, might get initialized from a call to a contract that is localized in a different chain from the funds.

For the moment, the standard will not include any specific functionality to facilitate such an action and relay on the usage of `permit2`. If, at some point in the future, these actions were to be included in the standard, a possible design could introduce a `remoteTransferFrom()` function.

Notice that this action is no longer atomic, so the contract would need a special implementation resume execution once the funds arrive in the target chain.

It would be necessary to add the following function to the interface

```solidity
function allowance(address _owner, address _spender, uint256 _chainId) external view returns (uint256);
function xApprove(address spender, uint256 chainId, uint256 amount) external returns (bool);
function remoteTransferFrom(
  address owner,
  address recipient,
  uint256 spendChainId,
  uint256 executeChainId,
  uint256 amount,
  bytes memory data
) external returns (bool);
function relayTransferFrom(
  address owner,
  address spender,
  address recipient,
  uint256 originChainId,
  uint256 executionChainId,
  uint256 amount,
  bytes memory data
) external;
function relayAndExecute(
  address owner,
  address recipient,
  address spender,
  uint256 originChainId,
  uint256 spenderChainId,
  uint256 amount,
  bytes memory _data
) external;
```

#### Approvals

To enable `remoteTransferFrom()`, it's necessary to include a separate `xAllowance` mapping that keeps track of the `chainId`. This allowance should be set locally using a new `xApprove()` function (the naming is open for discussion).

Here is what a possible implementation could look like:

```solidity
function xApprove(address _spender, uint256 _chainId, uint256 _amount) external returns (bool) {
  _xAllowances[msg.sender][_spender][_chainId] = _amount;
  emit XApproval(msg.sender, _spender, _chainId, _amount);
  return true;
}
// this will override the ERC20 view function
function allowance(address _owner, address _spender) external view returns (uint256) {
  return allowance(_owner, _spender, block.chainid);
}

function allowance(address _owner, address _spender, uint256 _chainId) external view returns (uint256) {
  if (_chainId == block.chainId) return _allowance[_owner][_spender];
  return _xAllowance[_owner][_spender][_chainId];
}
```

It is technically possible for the standard to also have remote approvals (initialize in chain A and approval for funds in chain B). This feature, however, was discarded as an address might have different owners in different chains (think of smart wallets for instance). Moreover, even if this was not an issue, switching `chainId` is not that relevant UX-wise to include a dedicated function in the token standard.

#### `remoteTransferFrom()`

As already mentioned, the `remoteTransferFrom()` will allow using funds on a remote chain on behalf of another party. The function should initiate a call to the `L2ToL2CrossChainMessenger` without burning anything on the origin chain.

To process a `remoteTransferFrom()` on the destination, the standard should include a `relayTransferFrom()` function that checks local allowance and calls `burn()`. Then, it will check if the target lives on the same `chainId` to call `relayAndExecute()` to the corresponding contract.

Finally, the standard should included a `relayAndExecute()` function that might be used to resume execution on the origin chain. This is how the atomicity lost in the `transferFrom()` gets addressed.

A possible implementation for these functionalities would look like this:

```solidity
function remoteTransferFrom(
  address _owner,
  address _recipient,
  uint256 _spendChainId,
  uint256 _executeChainId,
  uint256 _amount,
  bytes memory _data
) external returns (bool) {
  bytes memory _message =
    abi.encodeCall(this.relayTransferFrom, (_owner, msg.sender, _recipient, block.chainid, _executionChainId, _amount, _data));
  _sendMessage(_spendChainId, _message);
  return true;
}

function relayTransferFrom(
  address _owner,
  address _spender,
  address _recipient,
  uint256 _originChainId,
  uint256 _executionChainId,
  uint256 _amount,
  bytes memory _data
) external onlyMessenger {
  uint256 allowed = _xAllowances[_owner][_spender][_originChainId];

  if (allowed != type(uint256).max) {
    _xAllowances[_owner][_spender][_originChainId] = allowed - _amount;
  }

  _burn(_owner, _amount);

  if (block.chainid == _executionChainId) {
    _relayAndExecute(_owner, _recipient, _spender, _originChainId, block.chainid, _amount, _data);
  } else {
    bytes memory _message =
    abi.encodeCall(this.relayAndExecute, (_owner, _recipient, _spender, _originChainId, block.chainid, _amount, _data));
    _sendMessage(_executionChainId, _message);
  }

  emit RelayTransferFrom(_owner, _spender, _recipient, _originChainId, _executeChainId, _amount, _data);
}

function relayAndExecute(
  address _owner,
  address _spender,
  address _recipient,
  uint256 _originChainId,
  uint256 _spenderChainId,
  uint256 _amount,
  bytes memory _data
) external onlyMessenger {
  _relayAndExecute(_owner, _spender, _recipient, _originChainId, _spenderChainId, _amount, _data);
}

function _relayAndExecute(
  address _owner,
  address _spender,
  address _recipient,
  uint256 _originChainId,
  uint256 _spenderChainId,
  uint256 _amount,
  bytes memory _data
) internal {
  _mint(_recipient, _amount);
  if (_data.length > 0) {
    IxCallback(_spender).xCallback(_owner, _recipient, _spender, _originChainId, _spenderChainId, _amount, _data);
  }
  emit RelayAndExecute(_owner, _spender, _recipient, _originChainId, _spenderChainId, _amount, _data);
}

/////// Internals and modifiers ///////////
function _sendMessage(uint256 _destination, bytes memory _data) internal virtual {
  L2ToL2CrossDomainMessenger.sendMessage(_destination, address(this), _data);
}
modifier onlyMessenger() virtual {
  require(msg.sender == address(L2ToL2CrossChainMessenger));
  require(L2ToL2CrossChainMessenger.crossDomainMessageSender() == address(this));
  _;
}
```

You can check a full example implementation in the following gist.

https://gist.github.com/0xParticle/7ac85c7f32fa3364473f28fb9f6ebace
