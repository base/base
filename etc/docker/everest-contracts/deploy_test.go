package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"testing"

	"github.com/ethereum-optimism/optimism/op-chain-ops/foundry"
	"github.com/ethereum-optimism/optimism/op-deployer/pkg/deployer/state"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/log"
	"github.com/stretchr/testify/require"
)

func TestEverestContractGenesis(t *testing.T) {
	for i, role := range []string{"DEPLOYER_ADDR", "SEQUENCER_ADDR", "BATCHER_ADDR", "PROPOSER_ADDR", "CHALLENGER_ADDR"} {
		t.Setenv(role, fmt.Sprintf("0x%040x", 100+i))
	}
	intent, err := baseIntent("1337", 84538453)
	require.NoError(t, err)
	st := &state.State{Version: 1}
	require.NoError(t, deploy(intent, st, log.New(), ""))
	implSlot := common.HexToHash("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc")
	checkProxy := func(name string, alloc *foundry.ForgeAllocs, proxy, impl common.Address) {
		t.Helper()
		require.NotEmpty(t, alloc.Accounts[proxy].Code, name)
		require.Equal(t, common.BytesToHash(impl.Bytes()), alloc.Accounts[proxy].Storage[implSlot], name)
		artifact, err := foundry.ReadArtifact(filepath.Join(os.Getenv("BASE_DEVNET_ARTIFACTS"), name+".sol", name+".json"))
		require.NoError(t, err)
		got := alloc.Accounts[impl].Code
		want := bytes.Clone(artifact.DeployedBytecode.Object)
		require.NotEmpty(t, want, name)
		require.Len(t, got, len(want), name)
		// Constructors fill only these compiler-declared offsets at deployment.
		var refs map[string][]struct{ Start, Length int }
		if raw := artifact.DeployedBytecode.ImmutableReferences; len(raw) > 0 {
			require.NoError(t, json.Unmarshal(raw, &refs))
		}
		for _, offsets := range refs {
			for _, r := range offsets {
				end := r.Start + r.Length
				copy(want[r.Start:end], got[r.Start:end])
			}
		}
		require.True(t, bytes.Equal(want, got), "%s implementation differs from the release artifact", name)
	}
	chain := st.Chains[0]
	checkProxy("SystemConfig", st.L1StateDump.Data, chain.SystemConfigProxy, st.ImplementationsDeployment.SystemConfigImpl)
	checkProxy("L2StandardBridge", chain.Allocs.Data, common.HexToAddress("0x4200000000000000000000000000000000000010"), common.HexToAddress("0xc0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d30010"))
	checkProxy("BaseTime", chain.Allocs.Data, common.HexToAddress("0x4200000000000000000000000000000000000030"), common.HexToAddress("0xc0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d30030"))
}
