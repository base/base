# `base-builder`

The block builder for the Base Stack.

Under the hood, `base-builder` runs [base-builder-core](../../crates/builder/core/).

For transaction replay, start with `base-builder node --replay.clock-start-ms <u64>`
to anchor the process-wide wall clock at a recorded Unix epoch in milliseconds.
Time advances at real speed; omitting the flag keeps the system clock. Installation
happens before node startup and fails if a global clock was already installed.
