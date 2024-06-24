# Token Bridging

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->

- [Interface](#interface)
- [Interface](#interface-1)
- [Implementation](#implementation)
  - [Cross Chain transfers](#cross-chain-transfers)
  - [Cross Chain `transferFrom`](#cross-chain-transferfrom)
- [Deployment and migrations](#deployment-and-migrations)
  - [Factory](#factory)
  - [Migration to the Standard](#migration-to-the-standard)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

Unified ERC20 token bridging built on the messaging protocol can make tokens fungible and movable across the superchain. An issue with bridging is that if the security model is not standardized, the bridged assets are not fungible with each other depending on which bridge solution was used to facilitate the transfer between domains.

## Interface

The `ISuperchainERC20` interface creates a standard way to send tokens
between chains in the Superchain. In contrast to [xERC20](https://www.xerc20.com/) where the user calls
a bridge with `mint` and `burn` privileges on the cross-chain enabled
tokens, the user instead calls methods on the token contract directly.

In addition to standard bridging, these specs include functions that
allow contracts to be cross-chain interoperable. For example, a contract in chain A
can send pre-approved funds from a user in chain B to a contract in chain C.

```solidity
interface ISuperchainERC20 {
	// functions to bridge using interop
  function bridgeERC20(uint256 amount, uint256 chainId) external returns (bool success);
  function bridgeERC20To(address to, uint256 amount, uint256 chainId) external returns (bool success);
  function finalizeBridgeERC20(address to, uint256 amount) external;

  // functions to handle remote transfers
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
}
```

## Interface

An example implementation that depends on deterministic deployments across chains
for security is provided. This construction builds on top of the [L2ToL2CrossDomainMessenger][l2-to-l2]
for both replay protection and domain binding.

[l2-to-l2]: ./predeploys.md#l2tol2crossdomainmessenger

```solidity
contract OptimismSuperchainERC20 is ERC20, ISuperchainERC20 {
  constructor(string memory _name, string memory _symbol, uint256 _decimals) ERC20(_name, _symbol, _decimals) {}

  function bridgeERC20(uint256 _amount, uint256 _chainId) external {
    bridgeERC20To(msg.sender, _amount, _chainId);
  }

  function bridgeERC20To(address _to, uint256 _amount, uint256 _chainId) public {
    _burn(msg.sender, amount);

    L2ToL2CrossDomainMessenger.sendMessage({
      _destination: chainId,
      _target: address(this),
      _message: abi.encodeCall(this.finalizeBridgeERC20, (to, amount))
    });
  }

  function finalizeBridgeERC20(address _to, uint256 _amount) external {
    require(msg.sender == address(L2ToL2CrossChainMessenger));
    require(L2ToL2CrossChainMessenger.crossDomainMessageSender() == address(this));

    _mint(to, amount);
  }
}
```

[Disclaimer]

- Threat the following code as experimental. It will have errors and will change function names. The goal is to discuss functionalities.
- Current function names are open to discussion.

## Implementation

### Cross Chain transfers

The standard will include the following three external functions:

- `bridgeERC20(uint256 amount, uint256 chainId)`
- `bridgeERC20To(address to, uint256 amount, uint256 chainId)`
- `finalizeBridgeERC20(address to, uint256 amount)`

The first two functions will burn `amount` tokens and initialize a message to the `L2ToL2CrossChainMessenger` to mint the `amount` in the destination (`to` address in `chainId`).

`finalizeBridgeERC20(address to, uint256 amount)` will process incoming messages from the `L2ToL2CrossChainMessenger` initiated from the same token on a different `chainId` and mint `amount` to the `to` address.

A possible implementation would look like this:

```solidity
function bridgeERC20(uint256 _amount, uint256 _chainId) external returns (bool success) {
    success = bridgeERC20To(msg.sender, _amount, _chainId);
  }

function bridgeERC20To(address _to, uint256 _amount, uint256 _chainId) public returns (bool) {
  _burn(msg.sender, _amount);
  bytes memory _message = abi.encodeCall(this.finalizeBridgeERC20, (_to, _amount));
  _sendMessage(_chainId, _message);
  return true;
}

function finalizeBridgeERC20(address _to, uint256 _amount) external onlyMessenger {
  _mint(_to, _amount);
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

Note: `send()` or `xTransfer()` can also be good fits instead of `bridgeERC20()`.

### Cross Chain `transferFrom`

The standard will not include a remotely initialized `transfer` implementation as we think this should be handled directly on the chain where the funds live with the `bridgeERC20()` function. `transferFrom`, on the other hand, might get initialized from a call to a contract that is localized in a different chain from the funds. To be composable, the standard will introduce a `remoteTransferFrom()` function.

Notice that this action is no longer atomic, so the contract will need a special implementation to use it and resume execution once the funds arrive in the chain.

**Approvals**

To enable `remoteTransferFrom()`, we must include a separate `xAllowance` mapping that keeps track of the `chainId`. This allowance should be set locally using a new `xApprove()` function (the naming is open for discussion).

Here is what a possible implementation could look like:

```solidity
function xApprove(address _spender, uint256 _chainId, uint256 _amount) external returns (bool) {
  // TODO [research] _chainId wildcard as type(uint256).max (this is very risky)
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

It is possible for the standard to also have remote approvals. This feature, however, was discarded as an address might have different owners in different chains (think of smart wallets for instance). Moreover, even if this was not an issue, switching `chainId` is not that relevant UX-wise to include a dedicated function in the token standard.

**`remoteTransferFrom()`**

As already mentioned, the `remoteTransferFrom()` \*\*\*\*will allow using funds on a remote chain on behalf of another party. The function should initiate a call to the `L2ToL2CrossChainMessenger` without burning anything on the origin chain.

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

  if (block.chain == _executionChainId){
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

[TODO] We will add invariants.

## Deployment and migrations

### Factory

A token factory predeploy can ensure that `SuperchainERC20` tokens can be permissionlessly and deterministically deployed. Without a deterministic deployment scheme, maintaining a mapping of the token addresses between all of the chains will not be scalable.

[TODO] We will create a design doc that covers `SuperchainERC20` factories and link it in this specs.

### Migration to the Standard

- New tokens should implement the `SuperchainERC20` standard from inception.
- Tokens that are already deployed but upgradable, can update the implementation to be `SuperchainERC20` compatible.
- Tokens that are already deployed but are not upgradable should use methods such as Mirror (see https://github.com/ethereum-optimism/design-docs/pull/36).
  - An `OptimismMintableERC20Token` does not natively have the functionality
    to be sent between L2s. There are a few approaches to migrating L1 native tokens to L2 ERC20 tokens that support the `SuperchainERC20` interface that were covered in https://github.com/ethereum-optimism/design-docs/pull/35. The Mirror Standard is the preferred candidate now.
    To support L1 native tokens with cross chain liquidity, the Mirror Standard (or chosen solution) interface MUST be supported in addition to the `ISuperchainERC20` interface.
