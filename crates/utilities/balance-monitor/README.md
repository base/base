# `base-balance-monitor`

Periodic onchain balance monitoring for Base services.

Provides [`BalanceMonitorLayer`], an alloy [`ProviderLayer`] that spawns a
background task to poll an account's balance at a fixed interval and publish it
through a [`tokio::sync::watch`] channel. The layer reuses the existing
provider transport — no extra HTTP connection is needed.
