# base-ring-buffer

Generic bounded ring buffer for position-keyed replay.

Stores `(I, V)` entries in a fixed-capacity `VecDeque`. When capacity
is reached, the oldest entry is evicted on push.
