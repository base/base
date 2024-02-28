# Token Bridging

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Interface](#interface)
- [Implementation](#implementation)
- [L1 Native Tokens](#l1-native-tokens)
- [Cross Chain `transferFrom`](#cross-chain-transferfrom)
- [Factory](#factory)
- [Upgrading Existing `OptimismMintableERC20Token`s](#upgrading-existing-optimismmintableerc20tokens)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

Unified ERC20 token bridging built on top of the messaging protocol
can enable tokens to be fungible and movable across the superchain.
An issue with bridging is that if the security model is not standardized,
then the bridged assets are not fungible with each other depending on
which bridge solution was used to facilitate the transfer between
domains.

## Interface

The `ISuperchainERC20` interface creates a standard way to send tokens
between domains. In contrast to [xERC20][xerc20] where the user calls
a bridge that has `mint` and `burn` privileges on the cross chain enabled
tokens, the user instead calls methods on the token contract directly.

[xerc20]: https://www.xerc20.com/

```solidity
interface ISuperchainERC20 {
  function bridgeERC20(uint256 amount, uint256 chainId) external;
  function bridgeERC20To(address to, uint256 amount, uint256 chainId) external;
  function finalizeBridgeERC20(address to, uint256 amount) external;
}
```

## Implementation

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
      _message: abi.encodeCall(this.executeTransfer, (to, amount))
    });
  }

  function finalizeBridgeERC20(address to, uint256 amount) external {
    require(msg.sender == address(L2ToL2CrossChainMessenger));
    require(L2ToL2CrossChainMessenger.crossDomainMessageSender() == address(this));

    _mint(to, amount);
  }
}
```

## L1 Native Tokens

To support L1 native tokens with cross chain liquidity, the `OptimismMintableERC20Token`
interface MUST be supported in addition to the `SuperchainERC20` interface.

## Cross Chain `transferFrom`

Should an overloaded `approve` method be added to the interface so that a user can
approve a contract on a remote domain to bridge on their behalf? A new `allowances`
mapping could be added to the contract to enable this.

## Factory

A token factory predeploy can be used to ensure that `SuperchainERC20` tokens
can be permissionlessly and deterministically deployed. If a deterministic deployment
scheme is not used, maintaining a mapping of the token addresses between all of the
chains will not be scalable.

## Upgrading Existing `OptimismMintableERC20Token`s

An `OptimismMintableERC20Token` does not natively have the functionality
to be sent between L2s. There are a few approaches to migrating L1 native tokens
to L2 ERC20 tokens that support the `SuperchainERC20` interface.
