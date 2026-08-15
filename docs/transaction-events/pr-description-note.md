Transaction observability note:

This change introduces a dedicated transaction event journal path. Producers
append versioned `transaction-event/v1` JSONL records to a configured file for a
collector sidecar to tail. This is separate from stdout/stderr application logs,
which continue to use the normal Kubernetes Datadog path.
