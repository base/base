# `base-batcher-service-driver`

The live batcher: its driver state machine, submissions, throttling, administration endpoints,
configuration, and runtime composition. `BatcherService` starts the encoder, block source,
transaction manager, and `BatchDriver`.

The driver integration targets cover reorgs, throttling, head tracking, lifecycle, and pause/resume:

```sh
cargo test -p base-batcher-service-driver --features metrics,test-utils
```
