package metrics

import (
	"io"

	"github.com/ethereum/go-ethereum/log"

	opproposermetrics "github.com/ethereum-optimism/optimism/op-proposer/metrics"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	opmetrics "github.com/ethereum-optimism/optimism/op-service/metrics"
	txmetrics "github.com/ethereum-optimism/optimism/op-service/txmgr/metrics"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/prometheus/client_golang/prometheus"
)

const Namespace = "op_succinct_proposer"

// implements the Registry getter, for metrics HTTP server to hook into
var _ opmetrics.RegistryMetricer = (*OPSuccinctMetrics)(nil)

type OPSuccinctMetricer interface {
	opproposermetrics.Metricer

	RecordProposerStatus(metrics ProposerMetrics)
	RecordError(label string, num uint64)
	RecordProveFailure(reason string)
	RecordWitnessGenFailure(reason string)
}

type OPSuccinctMetrics struct {
	ns       string
	registry *prometheus.Registry
	factory  opmetrics.Factory

	opmetrics.RefMetrics
	txmetrics.TxMetrics
	opmetrics.RPCMetrics

	info prometheus.GaugeVec
	up   prometheus.Gauge

	NumProving     prometheus.Gauge
	NumWitnessGen  prometheus.Gauge
	NumUnrequested prometheus.Gauge

	L2FinalizedBlock               prometheus.Gauge
	LatestContractL2Block          prometheus.Gauge
	HighestProvenContiguousL2Block prometheus.Gauge

	ErrorCount *prometheus.CounterVec
	ProveFailures *prometheus.CounterVec
	WitnessGenFailures *prometheus.CounterVec
}

var _ OPSuccinctMetricer = (*OPSuccinctMetrics)(nil)

func NewMetrics(procName string) *OPSuccinctMetrics {
	if procName == "" {
		procName = "default"
	}
	ns := Namespace + "_" + procName

	registry := opmetrics.NewRegistry()
	factory := opmetrics.With(registry)

	return &OPSuccinctMetrics{
		ns:       ns,
			registry: registry,
			factory:  factory,

			RefMetrics: opmetrics.MakeRefMetrics(ns, factory),
			TxMetrics:  txmetrics.MakeTxMetrics(ns, factory),
			RPCMetrics: opmetrics.MakeRPCMetrics(ns, factory),

			info: *factory.NewGaugeVec(prometheus.GaugeOpts{
				Namespace: ns,
				Name:      "info",
				Help:      "Pseudo-metric tracking version and config info",
			}, []string{
				"version",
			}),
			up: factory.NewGauge(prometheus.GaugeOpts{
				Namespace: ns,
				Name:      "up",
				Help:      "1 if the op-proposer has finished starting up",
			}),
			NumProving: factory.NewGauge(prometheus.GaugeOpts{
				Namespace: ns,
				Name:      "num_proving",
				Help:      "Number of proofs currently being proven",
			}),
			NumWitnessGen: factory.NewGauge(prometheus.GaugeOpts{
				Namespace: ns,
				Name:      "num_witness_gen",
				Help:      "Number of witnesses currently being generated",
			}),
			NumUnrequested: factory.NewGauge(prometheus.GaugeOpts{
				Namespace: ns,
				Name:      "num_unrequested",
				Help:      "Number of unrequested proofs",
			}),
			L2FinalizedBlock: factory.NewGauge(prometheus.GaugeOpts{
				Namespace: ns,
				Name:      "l2_finalized_block",
				Help:      "Latest finalized L2 block number",
			}),
			LatestContractL2Block: factory.NewGauge(prometheus.GaugeOpts{
				Namespace: ns,
				Name:      "latest_contract_l2_block",
				Help:      "Latest L2 block number on the L2OO contract",
			}),
			HighestProvenContiguousL2Block: factory.NewGauge(prometheus.GaugeOpts{
				Namespace: ns,
				Name:      "highest_proven_contiguous_l2_block",
				Help:      "Highest proven L2 block contiguous with contract's latest block",
			}),
			ErrorCount: factory.NewCounterVec(prometheus.CounterOpts{
				Namespace: ns,
				Name:      "error_count",
				Help:      "Number of errors encountered",
			}, []string{"type"}),
			ProveFailures: factory.NewCounterVec(prometheus.CounterOpts{
				Namespace: ns,
				Name:      "prove_failures",
				Help:      "Number of prove failures by type",
			}, []string{"reason"}),
			WitnessGenFailures: factory.NewCounterVec(prometheus.CounterOpts{
				Namespace: ns,
				Name:      "witness_gen_failures",
				Help:      "Number of witness generation failures by type",
			}, []string{"reason"}),
	}
}

func (m *OPSuccinctMetrics) Registry() *prometheus.Registry {
	return m.registry
}

func (m *OPSuccinctMetrics) StartBalanceMetrics(l log.Logger, client *ethclient.Client, account common.Address) io.Closer {
	return opmetrics.LaunchBalanceMetrics(l, m.registry, m.ns, client, account)
}

// RecordInfo sets a pseudo-metric that contains versioning and
// config info for the op-proposer.
func (m *OPSuccinctMetrics) RecordInfo(version string) {
	m.info.WithLabelValues(version).Set(1)
}

// RecordUp sets the up metric to 1.
func (m *OPSuccinctMetrics) RecordUp() {
	prometheus.MustRegister()
	m.up.Set(1)
}

const (
	BlockProposed = "proposed"
)

// RecordL2BlocksProposed should be called when new L2 block is proposed
func (m *OPSuccinctMetrics) RecordL2BlocksProposed(l2ref eth.L2BlockRef) {
	m.RecordL2Ref(BlockProposed, l2ref)
}

func (m *OPSuccinctMetrics) Document() []opmetrics.DocumentedMetric {
	return m.factory.Document()
}

// RecordError records different types of errors
func (m *OPSuccinctMetrics) RecordError(errorType string, num uint64) {
	m.ErrorCount.WithLabelValues(errorType).Add(float64(num))
}

// RecordProveFailure records specific prove failure types
func (m *OPSuccinctMetrics) RecordProveFailure(reason string) {
	m.ProveFailures.WithLabelValues(reason).Inc()
}

// RecordWitnessGenFailure records specific witness generation failure types
func (m *OPSuccinctMetrics) RecordWitnessGenFailure(reason string) {
	m.WitnessGenFailures.WithLabelValues(reason).Inc()
}

// RecordProposerStatus sets the proposer Prometheus metrics to the given values.
func (m *OPSuccinctMetrics) RecordProposerStatus(metrics ProposerMetrics) {
	m.NumProving.Set(float64(metrics.NumProving))
	m.NumWitnessGen.Set(float64(metrics.NumWitnessgen))
	m.NumUnrequested.Set(float64(metrics.NumUnrequested))
	m.L2FinalizedBlock.Set(float64(metrics.L2FinalizedBlock))
	m.LatestContractL2Block.Set(float64(metrics.LatestContractL2Block))
	m.HighestProvenContiguousL2Block.Set(float64(metrics.HighestProvenContiguousL2Block))
}

type ProposerMetrics struct {
	L2UnsafeHeadBlock              uint64
	L2FinalizedBlock               uint64
	LatestContractL2Block          uint64
	HighestProvenContiguousL2Block uint64
	NumProving                     uint64
	NumWitnessgen                  uint64
	NumUnrequested                 uint64
}
