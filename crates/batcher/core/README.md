# base-batcher-core

Async orchestration loop for the Base batcher. `BatchDriver` combines a
`BatchPipeline` (encoding), an `UnsafeBlockSource` (L2 block delivery), and a
`TxManager` (L1 submission) into a single `tokio::select!` task with
`FuturesUnordered` receipt tracking and semaphore-based backpressure.
