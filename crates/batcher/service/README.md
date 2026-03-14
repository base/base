# `base-batcher-service`

Batcher service configuration and startup glue.

Provides [`BatcherConfig`] for full runtime configuration and [`BatcherService`]
which wires the encoder, block source, transaction manager, and driver into a
running batcher process.
