# `base-execution-state-maintenance`

Database initialization, state import, and maintenance queries. Shared storage and pruning
configuration lives in `base-execution-state-types`; maintenance does not depend on node configuration.

The ETL collector sorts buffered rows and spills them to temporary files for bounded-memory imports.

Pruning jobs and static-file receipt production share the provider layer and persistence schemas.
