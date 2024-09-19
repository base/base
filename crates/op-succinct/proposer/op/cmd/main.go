package main

import (
	"context"
	"fmt"
	"log"
	"os"

	"github.com/ethereum-optimism/optimism/op-service/dial"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/joho/godotenv"
	"github.com/succinctlabs/op-succinct-go/proposer/utils"
	"github.com/urfave/cli/v2"
)

func main() {
	if err := godotenv.Load(); err != nil {
		log.Println("No .env file found, proceeding with default values.")
	}

	app := &cli.App{
		Name:  "get-range",
		Usage: "For a given L2 block number, gets the full range of the span batch that it's a part of",
		Flags: []cli.Flag{
			&cli.Uint64Flag{
				Name:  "start",
				Usage: "The L2 block number to start at",
			},
			&cli.Uint64Flag{
				Name:  "end",
				Usage: "The L2 block number to end at",
			},
			&cli.StringFlag{
				Name:     "l2",
				Required: false,
				Usage:    "L2 RPC URL",
				EnvVars:  []string{"L2_RPC"},
			},
			&cli.StringFlag{
				Name:     "l2.node",
				Required: false,
				Usage:    "L2 node URL",
				EnvVars:  []string{"L2_NODE_RPC"},
			},
			&cli.StringFlag{
				Name:     "l1",
				Required: false,
				Usage:    "L1 RPC URL",
				EnvVars:  []string{"L1_RPC"},
			},
			&cli.StringFlag{
				Name:     "l1.beacon",
				Required: false,
				Usage:    "Address of L1 Beacon-node HTTP endpoint to use",
				EnvVars:  []string{"L1_BEACON_RPC"},
			},
			&cli.StringFlag{
				Name:     "sender",
				Required: false,
				Usage:    "Batch Sender Address",
			},
		},
		Action: func(cliCtx *cli.Context) error {
			// Get the chain ID from the L2 RPC.
			l2Client, err := ethclient.Dial(cliCtx.String("l2"))
			if err != nil {
				log.Fatal(err)
			}
			chainID, err := l2Client.ChainID(context.Background())
			if err != nil {
				log.Fatal(err)
			}

			// Load the rollup config for the given L2 chain ID.
			rollupCfg, err := utils.LoadOPStackRollupConfigFromChainID(chainID.Uint64())
			if err != nil {
				log.Fatal(err)
			}

			l1BeaconClient, err := utils.SetupBeacon(cliCtx.String("l1.beacon"))
			if err != nil {
				log.Fatal(err)
			}

			l1Client, err := ethclient.Dial(cliCtx.String("l1"))
			if err != nil {
				log.Fatal(err)
			}
			rollupClient, err := dial.DialRollupClientWithTimeout(cliCtx.Context, dial.DefaultDialTimeout, nil, cliCtx.String("l2.node"))
			if err != nil {
				log.Fatal(err)
			}

			config := utils.BatchDecoderConfig{
				L2GenesisTime:     rollupCfg.Genesis.L2Time,
				L2GenesisBlock:    rollupCfg.Genesis.L2.Number,
				L2BlockTime:       rollupCfg.BlockTime,
				BatchInboxAddress: rollupCfg.BatchInboxAddress,
				L2StartBlock:      cliCtx.Uint64("start"),
				L2EndBlock:        cliCtx.Uint64("end"),
				L2ChainID:         rollupCfg.L2ChainID,
				L2Node:            rollupClient,
				L1RPC:             *l1Client,
				L1Beacon:          l1BeaconClient,
				BatchSender:       rollupCfg.Genesis.SystemConfig.BatcherAddr,
				DataDir:           fmt.Sprintf("/tmp/batch_decoder/%d/transactions_cache", rollupCfg.L2ChainID),
			}

			ranges, err := utils.GetAllSpanBatchesInL2BlockRange(config)
			if err != nil {
				log.Fatal(err)
			}
			fmt.Printf("Span batch ranges: %v\n", ranges)
			return nil
		},
	}

	if err := app.Run(os.Args); err != nil {
		log.Fatal(err)
	}
}
