# Prewarm wiring plan (implemented)

Status: **wired**. `--prewarm` / `--prewarm-simulate` drive the production
`PrewarmWorkerPool` from `ReplayBuildBench::execute` (see `src/bench.rs`); the fail-closed
`ensure_prewarm_unwired` guard is gone, replaced by `validate_prewarm_flags`
(`--prewarm-simulate` requires `--prewarm`) and by `prewarm.active` in the JSON output.
This file is kept as the record of the plan; the two deviations the implementation had to
make are noted below.

## Production API (as of `builder-sim-prewarm/phase1-shared-pool`)

`crates/execution/payload/src/prewarm.rs`:

```rust
pub struct PrewarmConfig {
    pub enabled: bool,        // default false
    pub worker_count: usize,  // default 2
    pub lookahead: usize,     // default 64   (predicate-key lookahead)
    pub key_cap: usize,       // default 4096 (distinct keys per build)
    pub simulate: bool,       // default false
    pub sim_lookahead: usize, // default 16
}

impl PrewarmWorkerPool {
    /// Spawns `config.worker_count` detached worker threads once; a disabled
    /// config spawns nothing.
    pub fn new(config: &PrewarmConfig) -> Self;

    /// Starts one build's job, or `None` when disabled / all workers busy.
    /// `provider_factory` is cloned per dispatched worker and invoked *inside*
    /// that worker thread to open a provider for the job's exact parent state.
    pub fn try_start_job<F>(&self, provider_factory: F, cache: ExecutionCache)
        -> Option<PrewarmJob>
    where F: Fn() -> ProviderResult<StateProviderBox> + Clone + Send + 'static;
}

pub struct PrewarmJob { pub scheduler: Arc<PrewarmScheduler>, /* .. */ }

impl PrewarmingBestTransactions<I, T> {
    /// Opens its own lookahead cursor from a `TransactionPool`.
    pub fn new<P: TransactionPool<Transaction = T>>(
        inner: I, pool: P, attributes: BestTransactionsAttributes,
        prewarm: Option<Arc<PrewarmScheduler>>, sim: Option<SimSetup<T>>) -> Self;

    /// Wraps an already-opened lookahead cursor (used by the flashblocks builder).
    pub fn with_cursor(
        inner: I,
        cursor: Option<Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>>,
        prewarm: Option<Arc<PrewarmScheduler>>, sim: Option<SimSetup<T>>) -> Self;
}

pub struct SimSetup<T> { pub factory: Arc<SimJobFactory<T>>, pub lookahead: usize }
```

`crates/execution/payload/src/builder.rs` build loop (`BasePayloadBuilder::build_payload`):

1. `PrewarmWorkerPool::new(&config.prewarm)` once per builder, held as
   `Arc<PrewarmWorkerPool>` (threads spawn once, not per build).
2. Per build, skipped when `attributes.no_tx_pool()` or no shared execution cache:
   ```rust
   let prewarm = execution_cache.as_ref().and_then(|cache| {
       self.prewarm_pool.try_start_job(
           { let client = self.client.clone(); let parent = ctx.parent().hash();
             move || client.state_by_block_hash(parent) },
           cache.cache().clone(),
       )
   });
   ```
3. `let prewarm_scheduler = prewarm.as_ref().map(|job| Arc::clone(&job.scheduler));`
4. `let sim_setup = prewarm.as_ref().and_then(|_| ctx.simulation_setup());` — simulation
   warming only runs where predicate warming is already active.
5. The transaction selector closure wraps the inner iterator:
   `PrewarmingBestTransactions::new(inner, cursor_pool, attributes, prewarm_scheduler, sim_setup)`.
6. Dropping `PrewarmJob` after the build closes the scheduler queue and releases workers.

## Harness wiring after the rebase

In `bench.rs`:

1. Build the config from the CLI flags (`--prewarm`, `--prewarm-workers`,
   `--prewarm-simulate`, `--prewarm-sim-lookahead`); `lookahead`
   and `key_cap` keep `PrewarmConfig` defaults unless flags are added for them:
   ```rust
   let prewarm = PrewarmConfig {
       enabled: self.prewarm,
       worker_count: self.prewarm_workers,
       simulate: self.prewarm_simulate,
       sim_lookahead: self.prewarm_sim_lookahead,
       ..PrewarmConfig::default()
   };
   let builder_config = BaseBuilderConfig { prewarm, ..Default::default() };
   ```
   and pass `builder_config.clone()` into every `BasePayloadBuilderCtx` (today the harness
   uses `BaseBuilderConfig::default()`), so `ctx.simulation_setup()` sees `simulate`.
2. Create the pool **once**, before the replay loop (threads must not be respawned per
   block): `let prewarm_pool = PrewarmWorkerPool::new(&prewarm);`
3. Per block, before the timed build:
   ```rust
   let job = prewarm_pool.try_start_job(parent_state_factory.clone(), cache.clone());
   let scheduler = job.as_ref().map(|job| Arc::clone(&job.scheduler));
   let sim_setup = job.as_ref().and_then(|_| ctx.simulation_setup());
   ```
   and drop `job` after the build (end of iteration) so the scheduler queue closes.
4. The selector closure must use `with_cursor`, not `new`: the harness has no
   `TransactionPool`, only a `PendingPool` (`Self::pool_for`). Open a **second**,
   independent lookahead cursor from the same pending pool and wrap it the way
   `ParkableBestPayloadTransactions` wraps the main one:
   ```rust
   let mut lookahead = pool.best();
   lookahead.no_updates();
   PrewarmingBestTransactions::with_cursor(
       ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
           cursor, BaseOrdering::coinbase_tip(), attributes.basefee))),
       Some(Box::new(ParkedBestTransactions::new(
           lookahead, BaseOrdering::coinbase_tip(), attributes.basefee))),
       scheduler,
       sim_setup,
   )
   ```
   (Check that `ParkedBestTransactions` satisfies the
   `Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>` cursor bound; if not,
   pass the raw `pool.best()` cursor, which already yields
   `Arc<ValidPoolTransaction<_>>`.)

## Two blockers to resolve during the rebase

- **`ctx.simulation_setup()` is private** (`fn simulation_setup<T>(&self)` in
  `builder.rs`). The harness lives in another crate, so it must be made `pub` on the
  builder branch (preferred: it is a pure helper over `builder_config.prewarm` and the EVM
  env) or the harness has to duplicate its ~40 lines. Do not duplicate: the point of the
  harness is to measure the production path.
- **`try_start_job` needs an owned `'static` provider factory** returning a
  `StateProviderBox` per worker. The harness's canonical parent state is the durable
  overlay (`DurableStateProvider`, see `src/durable_state.rs`), which today owns its
  `RwLock<StateOverlay>` and a non-`Sync` anchor `StateProviderBox`, so it cannot be
  shared with worker threads as-is. Faithful fix: give `DurableStateProvider` an
  `Arc<RwLock<StateOverlay>>` and a `from_parts(anchor, overlay)` constructor, then the
  factory closure opens a fresh per-worker anchor from the (cloneable, `'static`)
  `ProviderFactory` and layers the shared overlay on it (**as implemented**; the anchor is
  *not* shared, because `StateProviderBox` is `Box<dyn StateProvider + Send>` and not
  `Sync`, so an `Arc<StateProviderBox>` would itself be `!Send` and could never reach a
  worker thread):
  ```rust
  let factory = factory.clone();
  let overlay = Arc::clone(&durable.overlay_handle());
  move || Ok(Box::new(DurableStateProvider::from_parts(
      factory.history_by_block_number(from)?, Arc::clone(&overlay))) as StateProviderBox)
  ```
  Overlay writes happen between blocks, while no job is running, so workers only ever take
  read guards. Note this gives each worker its own MDBX read transaction — same as
  production, where each worker calls `client.state_by_block_hash(parent)`.

## Implementation deviations

- **The builder config is still constructed per block** rather than cloned from one hoisted
  `BaseBuilderConfig` (step 1): `BaseBuilderConfig::clone` shares its `RejectionCache`
  (a `moka` cache), which would carry permanent rejections across replayed blocks and change
  the prewarm-off baseline. Only the `PrewarmConfig` (a `Copy` value) is hoisted.
- **The job is joined, not just dropped, after each build** (step 5): the harness advances
  the shared `ExecutionCache` itself (`insert_state`), where production relies on the engine
  refusing to advance a cache that still has extra handles. Without the untimed join a
  worker still reading parent state could write pre-block values back into the cache after
  canonical advancement and corrupt later blocks.

## ABBA arms once wired

- A: `--run --count <n>` (prewarm off; current behaviour).
- B: `--run --count <n> --prewarm --prewarm-simulate` (plus any `--prewarm-*` sizing).

Both arms must report `prewarm.wired == true` in the JSON output; a `false` there means
the arm measured nothing.
