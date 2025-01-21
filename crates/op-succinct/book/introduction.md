# OP Succinct

*Documentation for OP Stack rollup operators and developers looking to upgrade to a Type-1 ZK rollup.*

## Overview

OP Succinct transforms any OP Stack rollup into a [fully type-1 ZK rollup](https://vitalik.eth.limo/general/2022/08/04/zkevm.html) using SP1. This means your rollup can benefit from the security of zero-knowledge proofs while maintaining compatibility with the OP Stack ecosystem.

### Key Benefits

- **1 hour finality** secured by ZKPs, a dramatic improvement over the 7-day withdrawal window of standard OP Stack rollups.
- **Unlimited customization** for rollup modifications in pure Rust and easy maintainability.
- **Cost-effective proving** with an average cost of proving only fractions of cent per transaction (with an expected 5-10x improvement by EOY), thanks to SP1's blazing fast performance.

## Getting Started

1. Check the [Prerequisites](./quick-start/prerequisites.md) to ensure your environment is ready.
2. Run the [Cost Estimator](./quick-start/cost-estimator.md) to understand the resource requirements for proving.
3. Try [OP Succinct in Mock Mode](./quick-start/mock.md) for development.
4. Deploy [OP Succinct in Full Mode](./quick-start/full.md) for production.

## Support and Community

All of this has been possible thanks to close collaboration with the core team at [OP Labs](https://www.oplabs.co/).

**Ready to upgrade your rollup? [Contact us](https://docs.google.com/forms/d/e/1FAIpQLSd2Yil8TrU54cIuohH1WvDvbxTusyqh5rsDmMAtGC85-Arshg/viewform?ref=https://succinctlabs.github.io/op-succinct/) to get started with a Type-1 zkEVM rollup powered by SP1.**

## Documentation Structure

- **Quick Start**: Get up and running quickly with basic setup and configuration.
- **Advanced**: Detailed guides for production deployment and maintenance.
- **Contracts**: `OPSuccinctL2OutputOracle` contract documentation and deployment guides.
- **FAQ & Troubleshooting**: Common issues and their solutions.
