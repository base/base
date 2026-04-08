# `base-balance-monitor`

Block-driven onchain balance monitoring for Base services.

Provides [`BalanceMonitorLayer`], an alloy [`ProviderLayer`] that spawns a
background task to poll an account's balance on every new block and publish it
through a [`tokio::sync::watch`] channel. The layer reuses the existing
provider transport — no extra HTTP connection is needed.
