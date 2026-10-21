# `base-balance-monitor`

Periodic onchain balance monitoring for Base services.

Provides [`BalanceMonitorLayer`], an alloy [`ProviderLayer`] that spawns a
background task to poll an account's balance immediately, then at a fixed
interval. Successful reads publish through a [`tokio::sync::watch`] channel,
including zero and unchanged balances. Failed reads log a warning and do not
publish. The layer reuses the existing provider transport, so no extra HTTP
connection is needed.

The receiver starts with a placeholder value of zero. Wait for `changed()`
before reading it to distinguish the first observed balance from that placeholder.
