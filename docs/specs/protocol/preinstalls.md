# Preinstalls

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Safe](#safe)
- [SafeL2](#safel2)
- [MultiSend](#multisend)
- [MultiSendCallOnly](#multisendcallonly)
- [SafeSingletonFactory](#safesingletonfactory)
- [Multicall3](#multicall3)
- [Create2Deployer](#create2deployer)
- [CreateX](#createx)
- [Arachnid's Deterministic Deployment Proxy](#arachnids-deterministic-deployment-proxy)
- [Permit2](#permit2)
- [ERC-4337 v0.6.0 EntryPoint](#erc-4337-v060-entrypoint)
- [ERC-4337 v0.6.0 SenderCreator](#erc-4337-v060-sendercreator)
- [ERC-4337 v0.7.0 EntryPoint](#erc-4337-v070-entrypoint)
- [ERC-4337 v0.7.0 SenderCreator](#erc-4337-v070-sendercreator)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

[Preinstalled smart contracts](../glossary.md#preinstalled-contract-preinstall) exist on Optimism
at predetermined addresses in the genesis state. They are similar to precompiles but instead run
directly in the EVM instead of running native code outside of the EVM and are developed by third
parties unaffiliated with the Optimism Collective.

These preinstalls are commonly deployed smart contracts that are being placed at genesis for convenience.
It's important to note that these contracts do not have the same security guarantees
as [Predeployed smart contracts](../glossary.md#predeployed-contract-predeploy).

The following table includes each of the preinstalls.

| Name                                      | Address                                    |
| ----------------------------------------- | ------------------------------------------ |
| Safe                                      | 0x69f4D1788e39c87893C980c06EdF4b7f686e2938 |
| SafeL2                                    | 0xfb1bffC9d739B8D520DaF37dF666da4C687191EA |
| MultiSend                                 | 0x998739BFdAAdde7C933B942a68053933098f9EDa |
| MultiSendCallOnly                         | 0xA1dabEF33b3B82c7814B6D82A79e50F4AC44102B |
| SafeSingletonFactory                      | 0x914d7Fec6aaC8cd542e72Bca78B30650d45643d7 |
| Multicall3                                | 0xcA11bde05977b3631167028862bE2a173976CA11 |
| Create2Deployer                           | 0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2 |
| CreateX                           | 0xba5Ed099633D3B313e4D5F7bdc1305d3c28ba5Ed |
| Arachnid's Deterministic Deployment Proxy | 0x4e59b44847b379578588920cA78FbF26c0B4956C |
| Permit2                                   | 0x000000000022D473030F116dDEE9F6B43aC78BA3 |
| ERC-4337 v0.6.0 EntryPoint                | 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 |
| ERC-4337 v0.6.0 SenderCreator             | 0x7fc98430eaedbb6070b35b39d798725049088348 |
| ERC-4337 v0.7.0 EntryPoint                | 0x0000000071727De22E5E9d8BAf0edAc6f37da032 |
| ERC-4337 v0.7.0 SenderCreator             | 0xEFC2c1444eBCC4Db75e7613d20C6a62fF67A167C |

## Safe

[Implementation](https://github.com/safe-global/safe-contracts/blob/v1.3.0/contracts/GnosisSafe.sol)

Address: `0x69f4D1788e39c87893C980c06EdF4b7f686e2938`

A multisignature wallet with support for confirmations using signed messages based on ERC191.
Differs from [SafeL2](#safel2) by not emitting events to save gas.

## SafeL2

[Implementation](https://github.com/safe-global/safe-contracts/blob/v1.3.0/contracts/GnosisSafeL2.sol)

Address: `0xfb1bffC9d739B8D520DaF37dF666da4C687191EA`

A multisignature wallet with support for confirmations using signed messages based on ERC191.
Differs from [Safe](#safe) by emitting events.

## MultiSend

[Implementation](https://github.com/safe-global/safe-contracts/blob/v1.3.0/contracts/libraries/MultiSend.sol)

Address: `0x998739BFdAAdde7C933B942a68053933098f9EDa`

Allows to batch multiple transactions into one.

## MultiSendCallOnly

[Implementation](https://github.com/safe-global/safe-contracts/blob/v1.3.0/contracts/libraries/MultiSendCallOnly.sol)

Address: `0xA1dabEF33b3B82c7814B6D82A79e50F4AC44102B`

Allows to batch multiple transactions into one, but only calls.

## SafeSingletonFactory

[Implementation](https://github.com/safe-global/safe-singleton-factory/blob/v1.0.17/source/deterministic-deployment-proxy.yul)

Address: `0x914d7Fec6aaC8cd542e72Bca78B30650d45643d7`

Singleton factory used by Safe-related contracts based on
[Arachnid's Deterministic Deployment Proxy](#arachnids-deterministic-deployment-proxy).

The original library used a pre-signed transaction without a chain ID to allow deployment on different chains.
Some chains do not allow such transactions to be submitted; therefore, this contract will provide the same factory
that can be deployed via a pre-signed transaction that includes the chain ID. The key that is used to sign is
controlled by the Safe team.

## Multicall3

[Implementation](https://github.com/mds1/multicall/blob/v3.1.0/src/Multicall3.sol)

Address: `0xcA11bde05977b3631167028862bE2a173976CA11`

`Multicall3` has two main use cases:

- Aggregate results from multiple contract reads into a single JSON-RPC request.
- Execute multiple state-changing calls in a single transaction.

## Create2Deployer

[Implementation](https://github.com/mdehoog/create2deployer/blob/69b9a8e112b15f9257ce8c62b70a09914e7be29c/contracts/Create2Deployer.sol)

The `create2Deployer` is a nice Solidity wrapper around the CREATE2 opcode. It provides the following ABI.

```solidity
    /**
     * @dev Deploys a contract using `CREATE2`. The address where the
     * contract will be deployed can be known in advance via {computeAddress}.
     *
     * The bytecode for a contract can be obtained from Solidity with
     * `type(contractName).creationCode`.
     *
     * Requirements:
     * - `bytecode` must not be empty.
     * - `salt` must have not been used for `bytecode` already.
     * - the factory must have a balance of at least `value`.
     * - if `value` is non-zero, `bytecode` must have a `payable` constructor.
     */
    function deploy(uint256 value, bytes32 salt, bytes memory code) public;
    /**
     * @dev Deployment of the {ERC1820Implementer}.
     * Further information: https://eips.ethereum.org/EIPS/eip-1820
     */
    function deployERC1820Implementer(uint256 value, bytes32 salt);
    /**
     * @dev Returns the address where a contract will be stored if deployed via {deploy}.
     * Any change in the `bytecodeHash` or `salt` will result in a new destination address.
     */
    function computeAddress(bytes32 salt, bytes32 codeHash) public view returns (address);
    /**
     * @dev Returns the address where a contract will be stored if deployed via {deploy} from a
     * contract located at `deployer`. If `deployer` is this contract's address, returns the
     * same value as {computeAddress}.
     */
    function computeAddressWithDeployer(
        bytes32 salt,
        bytes32 codeHash,
        address deployer
    ) public pure returns (address);
```

Address: `0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2`

When Canyon activates, the contract code at `0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2` is set to
`0x6080604052600436106100435760003560e01c8063076c37b21461004f578063481286e61461007157806356299481146100ba57806366cfa057146100da57600080fd5b3661004a57005b600080fd5b34801561005b57600080fd5b5061006f61006a366004610327565b6100fa565b005b34801561007d57600080fd5b5061009161008c366004610327565b61014a565b60405173ffffffffffffffffffffffffffffffffffffffff909116815260200160405180910390f35b3480156100c657600080fd5b506100916100d5366004610349565b61015d565b3480156100e657600080fd5b5061006f6100f53660046103ca565b610172565b61014582826040518060200161010f9061031a565b7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe082820381018352601f90910116604052610183565b505050565b600061015683836102e7565b9392505050565b600061016a8484846102f0565b949350505050565b61017d838383610183565b50505050565b6000834710156101f4576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152601d60248201527f437265617465323a20696e73756666696369656e742062616c616e636500000060448201526064015b60405180910390fd5b815160000361025f576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820181905260248201527f437265617465323a2062797465636f6465206c656e677468206973207a65726f60448201526064016101eb565b8282516020840186f5905073ffffffffffffffffffffffffffffffffffffffff8116610156576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152601960248201527f437265617465323a204661696c6564206f6e206465706c6f790000000000000060448201526064016101eb565b60006101568383305b6000604051836040820152846020820152828152600b8101905060ff815360559020949350505050565b61014e806104ad83390190565b6000806040838503121561033a57600080fd5b50508035926020909101359150565b60008060006060848603121561035e57600080fd5b8335925060208401359150604084013573ffffffffffffffffffffffffffffffffffffffff8116811461039057600080fd5b809150509250925092565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052604160045260246000fd5b6000806000606084860312156103df57600080fd5b8335925060208401359150604084013567ffffffffffffffff8082111561040557600080fd5b818601915086601f83011261041957600080fd5b81358181111561042b5761042b61039b565b604051601f82017fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0908116603f011681019083821181831017156104715761047161039b565b8160405282815289602084870101111561048a57600080fd5b826020860160208301376000602084830101528095505050505050925092509256fe608060405234801561001057600080fd5b5061012e806100206000396000f3fe6080604052348015600f57600080fd5b506004361060285760003560e01c8063249cb3fa14602d575b600080fd5b603c603836600460b1565b604e565b60405190815260200160405180910390f35b60008281526020818152604080832073ffffffffffffffffffffffffffffffffffffffff8516845290915281205460ff16608857600060aa565b7fa2ef4600d742022d532d4747cb3547474667d6f13804902513b2ec01c848f4b45b9392505050565b6000806040838503121560c357600080fd5b82359150602083013573ffffffffffffffffffffffffffffffffffffffff8116811460ed57600080fd5b80915050925092905056fea26469706673582212205ffd4e6cede7d06a5daf93d48d0541fc68189eeb16608c1999a82063b666eb1164736f6c63430008130033a2646970667358221220fdc4a0fe96e3b21c108ca155438d37c9143fb01278a3c1d274948bad89c564ba64736f6c63430008130033`.

## CreateX

[Implementation](https://github.com/pcaversaccio/createx/blob/main/src/CreateX.sol)

Address: `0xba5Ed099633D3B313e4D5F7bdc1305d3c28ba5Ed`

CreateX introduces additional logic for deploying contracts using `CREATE`, `CREATE2` and `CREATE3`.
It adds [salt protection](https://github.com/pcaversaccio/createx#special-features) for sender and chainID
and includes a set of helper functions.

The `keccak256` of the CreateX bytecode is `0xbd8a7ea8cfca7b4e5f5041d7d4b17bc317c5ce42cfbc42066a00cf26b43eb53f`.

## Arachnid's Deterministic Deployment Proxy

[Implementation](https://github.com/Arachnid/deterministic-deployment-proxy/blob/v1.0.0/source/deterministic-deployment-proxy.yul)

Address: `0x4e59b44847b379578588920cA78FbF26c0B4956C`

This contract can deploy other contracts with a deterministic address on any chain using `CREATE2`. The `CREATE2`
call will deploy a contract (like `CREATE` opcode) but instead of the address being
`keccak256(rlp([deployer_address, nonce]))` it instead uses the hash of the contract's bytecode and a salt.
This means that a given deployer address will deploy the
same code to the same address no matter when or where they issue the deployment. The deployer is deployed
with a one-time-use account, so no matter what chain the deployer is on, its address will always be the same. This
means the only variables in determining the address of your contract are its bytecode hash and the provided salt.

Between the use of `CREATE2` opcode and the one-time-use account for the deployer, this contracts ensures
that a given contract will exist at the exact same address on every chain, but without having to use the
same gas pricing or limits every time.

## Permit2

[Implementation](https://github.com/Uniswap/permit2/blob/0x000000000022D473030F116dDEE9F6B43aC78BA3/src/Permit2.sol)

Address: `0x000000000022D473030F116dDEE9F6B43aC78BA3`

Permit2 introduces a low-overhead, next-generation token approval/meta-tx system to make token approvals easier,
more secure, and more consistent across applications.

## ERC-4337 v0.6.0 EntryPoint

[Implementation](https://github.com/eth-infinitism/account-abstraction/blob/v0.6.0/contracts/core/EntryPoint.sol)

Address: `0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789`

This contract verifies and executes the bundles of ERC-4337 v0.6.0
[UserOperations](https://www.erc4337.io/docs/understanding-ERC-4337/user-operation) sent to it.

## ERC-4337 v0.6.0 SenderCreator

[Implementation](https://github.com/eth-infinitism/account-abstraction/blob/v0.6.0/contracts/core/SenderCreator.sol)

Address: `0x7fc98430eaedbb6070b35b39d798725049088348`

Helper contract for [EntryPoint](#erc-4337-v060-entrypoint) v0.6.0, to call `userOp.initCode` from a "neutral" address,
which is explicitly not `EntryPoint` itself.

## ERC-4337 v0.7.0 EntryPoint

[Implementation](https://github.com/eth-infinitism/account-abstraction/blob/v0.7.0/contracts/core/EntryPoint.sol)

Address: `0x0000000071727De22E5E9d8BAf0edAc6f37da032`

This contract verifies and executes the bundles of ERC-4337 v0.7.0
[UserOperations](https://www.erc4337.io/docs/understanding-ERC-4337/user-operation) sent to it.

## ERC-4337 v0.7.0 SenderCreator

[Implementation](https://github.com/eth-infinitism/account-abstraction/blob/v0.7.0/contracts/core/SenderCreator.sol)

Address: `0xEFC2c1444eBCC4Db75e7613d20C6a62fF67A167C`

Helper contract for [EntryPoint](#erc-4337-v070-entrypoint) v0.7.0, to call `userOp.initCode` from a "neutral" address,
which is explicitly not `EntryPoint` itself.
