# base-ring-buffer

Generic bounded ring buffer for position-keyed replay.

Stores `(Option<I>, V)` entries in a fixed-capacity `VecDeque`. When
capacity is reached, the oldest entry is evicted on push.

Entries whose position is `None` are treated as sentinels — they are
always included when iterating after a given cutoff.
