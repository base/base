# `base-upgrade-signal`

Shared utilities for reading network upgrade activation signals from L1.

The crate reads an L1 contract interface and decodes the announced activation timestamp and expected
minimum node protocol version for configured hardfork IDs. It records metrics for startup schedule
reads and live signal changes without mutating schedules after startup. Callers validate the
minimum protocol version, reject positive timestamps without one, then use the decoded schedule to
populate timestamp-based hardfork activation config at startup.

See [`docs/guides/UPGRADE_SIGNAL.md`](../../../docs/guides/UPGRADE_SIGNAL.md) for the feature
overview and code-path guide.
