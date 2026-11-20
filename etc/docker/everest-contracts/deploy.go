// Compiled into the Everest setup image in place of op-deployer's OP-specific
// deployment phase. Genesis headers, rollup config and beacon setup are shared.
package main

import (
	"encoding/json"
	"fmt"
	"math/big"
	"os"

	"github.com/ethereum-optimism/optimism/op-chain-ops/addresses"
	"github.com/ethereum-optimism/optimism/op-chain-ops/foundry"
	"github.com/ethereum-optimism/optimism/op-chain-ops/script"
	"github.com/ethereum-optimism/optimism/op-deployer/pkg/deployer/broadcaster"
	"github.com/ethereum-optimism/optimism/op-deployer/pkg/deployer/opcm"
	"github.com/ethereum-optimism/optimism/op-deployer/pkg/deployer/pipeline"
	"github.com/ethereum-optimism/optimism/op-deployer/pkg/deployer/standard"
	"github.com/ethereum-optimism/optimism/op-deployer/pkg/deployer/state"
	scriptenv "github.com/ethereum-optimism/optimism/op-deployer/pkg/env"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/log"
)

type devnetL1 struct {
	Build func(*big.Int, *big.Int, common.Address, common.Address, common.Address, uint64, [32]byte, uint32, uint32) (string, error)
}

type devnetL2 struct {
	Build func(*big.Int, *big.Int, common.Address, common.Address, common.Address, common.Address, *big.Int, bool) error
}

func deploy(intent *state.Intent, st *state.State, logger log.Logger, _ string) error {
	if err := intent.Check(); err != nil {
		return err
	}
	if len(intent.Chains) != 1 {
		return fmt.Errorf("Everest requires a single-chain intent")
	}
	chain := intent.Chains[0]
	artifactDir := os.Getenv("BASE_DEVNET_ARTIFACTS")
	if artifactDir == "" {
		return fmt.Errorf("missing pinned Everest contract artifacts")
	}
	fs := os.DirFS(artifactDir).(foundry.StatDirFs)
	deployer := common.Address{0x01}
	newHost := func() (*script.Host, error) {
		return scriptenv.DefaultScriptHost(broadcaster.NoopBroadcaster(), logger, deployer, fs)
	}
	l1, err := newHost()
	if err != nil {
		return err
	}
	if err := pipeline.InitGenesisStrategy(&pipeline.Env{Logger: logger}, intent, st); err != nil {
		return err
	}
	l1Script, cleanup, err := script.WithScript[devnetL1](l1, "DevnetGenesis.s.sol", "DevnetL1")
	if err != nil {
		return err
	}
	encoded, err := l1Script.Build(intent.L1ChainIDBig(), chain.ID.Big(), chain.Roles.L1ProxyAdminOwner, chain.Roles.Batcher, chain.Roles.UnsafeBlockSigner, chain.GasLimit, st.Create2Salt, standard.BasefeeScalar, standard.BlobBaseFeeScalar)
	cleanup()
	if err != nil {
		return fmt.Errorf("deploy Base L1 contracts: %w", err)
	}
	var contracts addresses.L1Contracts
	if err := json.Unmarshal([]byte(encoded), &contracts); err != nil {
		return fmt.Errorf("decode Base L1 deployment: %w", err)
	}
	if err := opcm.InsertPreinstalls(l1); err != nil {
		return err
	}
	l1.Wipe(deployer)
	l1Dump, err := l1.StateDump()
	if err != nil {
		return err
	}
	l2, err := newHost()
	if err != nil {
		return err
	}
	l2Script, cleanup, err := script.WithScript[devnetL2](l2, "DevnetGenesis.s.sol", "DevnetL2")
	if err != nil {
		return err
	}
	err = l2Script.Build(intent.L1ChainIDBig(), chain.ID.Big(), chain.Roles.L2ProxyAdminOwner, contracts.L1CrossDomainMessengerProxy, contracts.L1StandardBridgeProxy, contracts.L1Erc721BridgeProxy, standard.VaultMinWithdrawalAmount.ToInt(), intent.FundDevAccounts)
	cleanup()
	if err != nil {
		return fmt.Errorf("initialize Base L2 predeploys: %w", err)
	}
	l2.Wipe(deployer)
	l2Dump, err := l2.StateDump()
	if err != nil {
		return err
	}
	st.SuperchainDeployment = &contracts.SuperchainContracts
	st.ImplementationsDeployment = &contracts.ImplementationsContracts
	st.Chains = []*state.ChainState{{ID: chain.ID, OpChainContracts: contracts.OpChainContracts, Allocs: &state.GzipData[foundry.ForgeAllocs]{Data: l2Dump}}}
	st.L1StateDump = &state.GzipData[foundry.ForgeAllocs]{Data: l1Dump}
	st.AppliedIntent = intent
	return nil
}
