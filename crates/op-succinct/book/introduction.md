# OP Succinct

<a href="https://github.com/succinctlabs/op-succinct/actions/workflows/docker-build.yaml"><img src="https://img.shields.io/github/actions/workflow/status/succinctlabs/op-succinct/docker-build.yaml?style=flat&labelColor=1C2C2E&label=ci&color=BEC5C9&logo=GitHub%20Actions&logoColor=BEC5C9" alt="CI"></a>
   <a href="https://github.com/succinctlabs/op-succinct/blob/main/LICENSE-MIT"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=googledocs&label=license&logoColor=BEC5C9" alt="License"></a>
   <a href="https://succinctlabs.github.io/op-succinct"><img src="https://img.shields.io/badge/Book-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Book"></a>

# Overview

OP Succinct is the production-grade proving engine for the OP Stack, powered by [SP1](https://docs.succinct.xyz/docs/sp1/introduction) and the [Succinct Prover Network](https://docs.succinct.xyz/docs/network/introduction).

## Proving Options

OP Succinct provides two new proving options for the OP Stack: OP Succinct (Validity) and OP Succinct Lite.

| | OP Succinct (Validity) | OP Succinct Lite | Standard OP Stack |
|---------|-------------|------------------|-------------------|
| Proof System | Full validity proofs with SP1 | ZK fault proofs with SP1 (optional fast finality) | Interactive fraud proofs |
| Time to Finality | < 1 hour | 7 days* | 7 days |
| Security Level | Highest | High | High |
| Alt DA Support | ✅ | ✅ | ❌ |
| Dispute Process | Prove every transcation | Prove when there is a dispute | Replay transactions on L1 with FPVM |
| Dispute Capital Requirements | 0 | Configurable: 5-15 ETH (Less than worst-case prover cost) | Scales with TVL; up to 1000s of ETH |
| Ongoing proving costs | Less than a tenth of a cent per transaction; can be paid by users | $0; Proofs only generated with disputes | $0 |
| Designed for | Large L2s, DeFi Chains, and other high-value chains | Cost-sensitive L2s, appchains and gaming chains | When ZK was expensive |


## Support and Community

All of this has been possible thanks to close collaboration with the core team at [OP Labs](https://www.oplabs.co/).

**Ready to upgrade your rollup with ZK? [Contact us](https://docs.google.com/forms/d/e/1FAIpQLSd2Yil8TrU54cIuohH1WvDvbxTusyqh5rsDmMAtGC85-Arshg/viewform?ref=https://succinctlabs.github.io/op-succinct/) to get started with OP Succinct.**
