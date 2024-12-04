# Using Kurtosis to test `op-succinct`

## What is Kurtosis?

Kurtosis is a development tool that allows you spin up local testnets for blockchain development and testing. The ethPandaOps team has created a package for spinning up OP Stack devnets using Kurtosis called [optimism-package](https://github.com/ethpandaops/optimism-package).

## Install Kurtosis

First, install Kurtosis by following the instructions here: [Kurtosis Installation Guide](https://docs.kurtosis.com/install/).

## How to configure the OP Stack devnet

Configure the `op-network.yaml` file to use the Kurtosis engine:

```yaml
optimism_package:
  chains:
    - participants:
        - el_type: op-geth
          cl_type: op-node
      network_params:
        fjord_time_offset: 0
        granite_time_offset: 0
        holocene_time_offset: 0
      additional_services:
        - blockscout
ethereum_package:
  participants:
    - el_type: geth
    - el_type: reth
  network_params:
    preset: minimal
  additional_services:
    - blockscout
```

## How to run Kurtosis?

Run the testnet using the following command:

```bash
kurtosis run --enclave my-testnet github.com/ethpandaops/optimism-package@bbusa/add-miner --args-file op-network.yaml --image-download always
```

Note: We're currently using the `bbusa/add-miner` branch of the `optimism-package` repo because it has a fix for the `op-batcher` container.

## How to get the relevant RPC's from Kurtosis?

Once the Kurtosis service is running, you can get the relevant RPC endpoints (`L1_RPC`, `L2_RPC`, `L1_BEACON_RPC`, `L2_NODE_RPC`) from the logs:

```bash
========================================== User Services ==========================================
UUID           Name                                             Ports                                         Status
f4d46dd9d329   cl-1-lighthouse-geth                             http: 4000/tcp -> http://127.0.0.1:32940      RUNNING
                                                                metrics: 5054/tcp -> http://127.0.0.1:32941   
                                                                tcp-discovery: 9000/tcp -> 127.0.0.1:32942    
                                                                udp-discovery: 9000/udp -> 127.0.0.1:32796    
e42d898efb2e   el-1-geth-lighthouse                             engine-rpc: 8551/tcp -> 127.0.0.1:32937       RUNNING
                                                                metrics: 9001/tcp -> http://127.0.0.1:32938   
                                                                rpc: 8545/tcp -> 127.0.0.1:32935              
                                                                tcp-discovery: 30303/tcp -> 127.0.0.1:32939   
                                                                udp-discovery: 30303/udp -> 127.0.0.1:32795   
                                                                ws: 8546/tcp -> 127.0.0.1:32936               
37ed2311790f   op-batcher-op-kurtosis                           http: 8548/tcp -> http://127.0.0.1:32951      RUNNING
d068303cf7af   op-cl-1-op-node-op-geth-op-kurtosis              http: 8547/tcp -> http://127.0.0.1:32949      RUNNING
                                                                tcp-discovery: 9003/tcp -> 127.0.0.1:32950    
                                                                udp-discovery: 9003/udp -> 127.0.0.1:32798    
d2a8cecbf572   op-el-1-op-geth-op-node-op-kurtosis              engine-rpc: 8551/tcp -> 127.0.0.1:32946       RUNNING
                                                                metrics: 9001/tcp -> 127.0.0.1:32947          
                                                                rpc: 8545/tcp -> http://127.0.0.1:32944       
                                                                tcp-discovery: 30303/tcp -> 127.0.0.1:32948   
                                                                udp-discovery: 30303/udp -> 127.0.0.1:32797   
                                                                ws: 8546/tcp -> 127.0.0.1:32945               
7a6d8bc60601   validator-key-generation-cl-validator-keystore   <none>                                        RUNNING
bc47bef086de   vc-1-geth-lighthouse                             metrics: 8080/tcp -> http://127.0.0.1:32943   RUNNING
```

Relevant endpoints:

| Endpoint | Service | URL |
|----------|---------|-----|
| L1_RPC | `rpc` port of `el-1-geth-lighthouse` | `http://127.0.0.1:32935` |
| L2_RPC | `rpc` port of `op-el-1-op-geth-op-node-op-kurtosis` | `http://127.0.0.1:32944` |
| L1_BEACON_RPC | `http` port of `cl-1-lighthouse-geth` | `http://127.0.0.1:32940` |
| L2_NODE_RPC | `http` port of `op-cl-1-op-node-op-geth-op-kurtosis` | `http://127.0.0.1:32949` |

## Spin down the devnet

Remove the devnet with:

```bash
kurtosis clean -a
```