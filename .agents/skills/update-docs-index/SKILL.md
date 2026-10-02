---
name: update-docs-index
description: "Update llms.txt and llms-full.txt, the repo's index of every Markdown doc, by hand. Normally the docs-index autonomous agent does this; use this skill to reproduce or debug it locally."
---

# Update the docs index

`llms.txt` lists every Markdown doc with a one-line summary. `llms-full.txt` adds a repository map and conventions above the same index. To find a doc, read `llms.txt` instead of grepping the tree.

Do not edit the index regions of either file by hand. They are generated from `etc/docs-index.toml`, which holds one `summary` and one `digest` per doc. Run everything from the repository root.

## Steps

1. Run `python3 etc/scripts/local/docs-index.py check` to list what is out of date.
2. Fix each problem:
   - **New doc:** run `python3 etc/scripts/local/docs-index.py sync`. It adds the doc to the manifest with a `TODO: summarize` summary. Read the doc and replace that summary in `etc/docs-index.toml`.
   - **Edited doc:** re-read the doc and its summary. Edit the summary if it is no longer accurate. Then run `python3 etc/scripts/local/docs-index.py stamp <path>` to record the doc's new digest. Never stamp a doc you have not re-read.
   - **Deleted or moved doc:** run `sync`. It drops the dead entry and adds the new path with a placeholder to fill in.
   - **Edited the manifest by hand:** run `python3 etc/scripts/local/docs-index.py generate`.
3. Run `check` again until it prints `ok`. `sync`, `stamp`, and `generate` rewrite `llms.txt` and `llms-full.txt`, so commit those with the manifest.

## Writing a summary

- One sentence, at most 200 characters, with no newlines.
- Say what the doc covers and when someone would need it, not a restatement of the title. Name the commands, types, protocol, or decision.
- Claim only what the doc says. If a README is a stub, say so.

## Optional: second opinion

`python3 etc/scripts/local/docs-index.py validate [paths...]` asks a small model on the LLM gateway (needs `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`) whether each summary is supported by its doc. It is advisory and not part of CI. Its answers vary between runs, so re-read the doc before changing a summary because of a flag.

## Hand-written context in llms-full.txt

Text between `<!-- LLMS_EXTRAS_START -->` and `<!-- LLMS_EXTRAS_END -->` in `llms-full.txt` is preserved verbatim on every regeneration. Edit it there when the repository layout, common commands, or conventions change. Everything between the `LLMS_AUTOGEN` markers is overwritten.

No pre-merge check runs `check`. The `docs-index` autonomous agent (`.github/agents/README.md`) runs these same steps on `main` and keeps one pull request open until the index is current. Feature PRs should leave the index alone.
