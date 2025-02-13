SHELL := /bin/bash

.PHONY: bindings

bindings:
	@echo "Generating bindings for OPSuccinctL2OutputOracle.sol..."
	@mkdir -p bin generated_bindings
	@cd ../../contracts/ && forge install && forge build
	@cd ../../contracts/ && forge inspect src/validity/OPSuccinctL2OutputOracle.sol:OPSuccinctL2OutputOracle abi > ../proposer/op/generated_bindings/OPSuccinctL2OutputOracle.abi
	@cd ../../contracts/ && forge inspect src/validity/OPSuccinctL2OutputOracle.sol:OPSuccinctL2OutputOracle bytecode > ../proposer/op/generated_bindings/OPSuccinctL2OutputOracle.bin
	@abigen --abi generated_bindings/OPSuccinctL2OutputOracle.abi --bin generated_bindings/OPSuccinctL2OutputOracle.bin --pkg bindings --type OPSuccinctL2OutputOracle --out ./bindings/opsuccinctl2outputoracle.go
	@cd ../../contracts/ && forge inspect lib/optimism/packages/contracts-bedrock/src/dispute/DisputeGameFactory.sol:DisputeGameFactory abi > ../proposer/op/generated_bindings/DisputeGameFactory.abi
	@cd ../../contracts/ && forge inspect lib/optimism/packages/contracts-bedrock/src/dispute/DisputeGameFactory.sol:DisputeGameFactory bytecode > ../proposer/op/generated_bindings/DisputeGameFactory.bin
	@abigen --abi generated_bindings/DisputeGameFactory.abi --bin generated_bindings/DisputeGameFactory.bin --pkg bindings --type DisputeGameFactory --out ./bindings/disputegamefactory.go
	@rm -rf generated_bindings
	@echo "Bindings generated successfully."
