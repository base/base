# Feature Map

This is the index an agent reads before using Base's existing tools. Each linked page explains a behavior, how to exercise it, and what evidence establishes success.

| Feature | Expected behavior | Tools |
| --- | --- | --- |
| [Local devnet transaction inclusion](local-devnet-transaction-inclusion.md) | A funded ETH transfer sent directly to the local sequencer succeeds and appears in its reported block. | `just devnet up-single`, `cast send`, `cast block` |

This map covers each observable behavior that has a verification page, not only the row above; new rows are added as behaviors are discovered. The map is maintained with the commands and behavior each page describes, not generated from package counts or file/path heuristics. A row may originate from a default-off, human-reviewed automated draft (see `docs/runbooks/feature-docs-draft.md`); every row, generated or hand-authored, is only as trustworthy as the page it links to and the review it received.
