SHELL := /bin/bash

.PHONY: bindings

bindings:
	@echo "Generating bindings for ZKL2OutputOracle.sol..."
	@mkdir -p bin generated_bindings
	@cd ../contracts/ && forge install && forge build
	@cd ../contracts/ && forge inspect src/ZKL2OutputOracle.sol:ZKL2OutputOracle abi > ../proposer/op/generated_bindings/ZKL2OutputOracle.abi
	@cd ../contracts/ && forge inspect src/ZKL2OutputOracle.sol:ZKL2OutputOracle bytecode > ../proposer/op/generated_bindings/ZKL2OutputOracle.bin
	@abigen --abi generated_bindings/ZKL2OutputOracle.abi --bin generated_bindings/ZKL2OutputOracle.bin --pkg bindings --type ZKL2OutputOracle --out ./bindings/zkl2outputoracle.go
	@rm -rf generated_bindings
	@echo "Bindings generated successfully."
