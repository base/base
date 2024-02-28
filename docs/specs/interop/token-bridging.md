# Token Bridging

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Token Bridging](#token-bridging)
  - [Interface](#interface)
  - [Implementation](#implementation)
    - [L1 Native Tokens](#l1-native-tokens)

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
  function initiateTransfer(address to, uint256 amount, uint256 chainId) external;
  function executeTransfer(address to, uint256 amount) external;
}
```

The `initiate` and `execute` language is used to keep standard language with
the lower level messaging specification. Note that this interface can be used
for both OP Stack native interoperability and application layer interoperability.

## Implementation

An example implementation that depends on deterministic deployments across chains
for security is provided.

```solidity
contract OptimismSuperchainERC20 is ERC20, ISuperchainERC20 {
  function initiateTransfer(address to, uint256 amount, uint256 chainId) external {
    _burn(msg.sender, amount);

    L2ToL2CrossDomainMessenger.sendMessage({
      _destination: chainId,
      _target: address(this),
      _message: abi.encodeCall(this.executeTransfer, (to, amount))
    });
  }

  function executeTransfer(address to, uint256 amount) external {
    require(msg.sender == address(L2ToL2CrossChainMessenger));
    require(L2ToL2CrossChainMessenger.crossDomainMessageSender() == address(this));

    _mint(to, amount);
  }
}
```

### L1 Native Tokens

To support L1 native tokens with cross chain liquidity, the `OptimismMintableERC20Token`
interface MUST be supported in addition to the `SuperchainERC20` interface.
