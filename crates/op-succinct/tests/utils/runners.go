package utils

import (
	"os"
	"os/signal"
	"syscall"
	"time"
)

// Test timeouts.
func ShortTimeout() time.Duration { return 20 * time.Minute }
func LongTimeout() time.Duration  { return 40 * time.Minute }

// MaxProposerLag returns the maximum allowed lag between L2 finalized and proposer submissions.
func MaxProposerLag() uint64 {
	if UseNetworkProver() {
		return 900 // ~30m at 2s block time
	}
	return 300 // ~10m at 2s block time
}

// RunProgressTest runs onTick every 10 seconds for a configurable duration.
// Duration is controlled by PROGRESS_TEST_DURATION env var (default: 15m).
// Returns nil if successful, or the first error encountered.
func RunProgressTest(onTick func() error) error {
	duration := 15 * time.Minute
	if env := os.Getenv("PROGRESS_TEST_DURATION"); env != "" {
		if d, err := time.ParseDuration(env); err == nil {
			duration = d
		}
	}
	return RunForDuration(duration, 10*time.Second, onTick)
}

// RunForDuration runs onTick periodically for the specified duration.
// If onTick returns an error, the loop terminates early and returns that error.
// Returns nil if the full duration completes without errors.
func RunForDuration(duration, interval time.Duration, onTick func() error) error {
	deadline := time.NewTimer(duration)
	defer deadline.Stop()

	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-deadline.C:
			return nil
		case <-ticker.C:
			if err := onTick(); err != nil {
				return err
			}
		}
	}
}

// RunUntilShutdown runs onTick periodically until SIGINT/SIGTERM is received.
// If onTick returns an error, the loop terminates and returns that error.
// Returns nil on graceful shutdown via signal.
func RunUntilShutdown(interval time.Duration, onTick func() error) error {
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	defer signal.Stop(sigCh)

	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-sigCh:
			return nil
		case <-ticker.C:
			if err := onTick(); err != nil {
				return err
			}
		}
	}
}
