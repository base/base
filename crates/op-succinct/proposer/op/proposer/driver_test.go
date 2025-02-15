package proposer

import (
	"testing"

	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/stretchr/testify/require"
	opsuccinctbindings "github.com/succinctlabs/op-succinct-go/bindings"
)

func TestProposeL2OutputTxData(t *testing.T) {
	output := &eth.OutputResponse{
		OutputRoot: eth.Bytes32{0x01},
		BlockRef: eth.L2BlockRef{
			Number: 100,
		},
	}
	proof := []byte{0x02}
	l1BlockNum := uint64(200)

	l2ooAbiParsed, err := opsuccinctbindings.OPSuccinctL2OutputOracleMetaData.GetAbi()
	if err != nil {
		t.Fatalf("failed to get abi: %v", err)
	}

	_, err = proposeL2OutputTxData(l2ooAbiParsed, output, proof, l1BlockNum)
	require.NoError(t, err)

}
