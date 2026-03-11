# base-ring-buffer

A generic fixed-capacity ring buffer of sequenced entries.

Each entry carries an optional position of type `I` and a payload of type `V`.
Entries pushed with `position = None` are treated as sentinels and are always
returned by `entries_after`, regardless of the requested cutoff.

When the capacity is exceeded the oldest entry is evicted.
