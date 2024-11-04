package metrics

import (
	"io"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/ethereum/go-ethereum/log"

	"github.com/ethereum-optimism/optimism/op-service/eth"
	opmetrics "github.com/ethereum-optimism/optimism/op-service/metrics"
	txmetrics "github.com/ethereum-optimism/optimism/op-service/txmgr/metrics"
)

type noopMetrics struct {
	opmetrics.NoopRefMetrics
	txmetrics.NoopTxMetrics
	opmetrics.NoopRPCMetrics
}

var NoopMetrics OPSuccinctMetricer = new(noopMetrics)

func (*noopMetrics) RecordProposerStatus(metrics ProposerMetrics) {}
func (*noopMetrics) RecordError(label string, num uint64)         {}
func (*noopMetrics) RecordProveFailure(reason string)             {}
func (*noopMetrics) RecordWitnessGenFailure(reason string)        {}

func (*noopMetrics) RecordInfo(version string) {}
func (*noopMetrics) RecordUp()                 {}

func (*noopMetrics) RecordL2BlocksProposed(l2ref eth.L2BlockRef) {}

func (*noopMetrics) StartBalanceMetrics(log.Logger, *ethclient.Client, common.Address) io.Closer {
	return nil
}
