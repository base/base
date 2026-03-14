# witness-diff

`witness-diff` compares two execution witnesses to identify exactly which trie nodes, bytecodes, or key preimages differ between them. The primary use case is investigating state root mismatches: when two nodes disagree on the post-block state root for the same block, this tool locates the specific accounts and storage slots that diverge rather than requiring you to diff multi-megabyte JSON blobs manually.

An execution witness is the set of all Merkle trie node preimages, contract bytecodes, and key preimages that were read during the execution of a block. Two nodes that execute the same block identically will produce identical witnesses. If they disagree on the state root, their witnesses will differ in the leaf nodes corresponding to the accounts or storage slots that were computed differently.

## Enabling execution witnesses in reth

Execution witnesses are served via the `debug_executionWitness` JSON-RPC method, which is part of the debug namespace. To expose it, start reth with `--http.api debug` (or `--ws.api debug` for WebSocket). The method takes a block number in hex and returns the full witness for that block as re-executed by the node.

To additionally record witnesses automatically when a bad block is encountered and compare them against a healthy reference node, use `--debug.invalid-block-hook witness` together with `--debug.healthy-node-rpc-url <URL>`. That flow is handled by reth internally, but `witness-diff` is useful for the same underlying problem when you want to drive the comparison yourself.

## Obtaining a local witness

The local witness is fetched from your node using the same `debug_executionWitness` RPC method and saved to a JSON file before running the diff. For example:

```
curl -s -X POST http://localhost:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"debug_executionWitness","params":["0x292FA96"],"id":1}' \
  | jq '.result' > witness_43127830.json
```

The block number in the `params` array must be hex. The `.result` field of the response is the witness object. Save it as-is — `witness-diff` will parse it directly.

## Running the tool

```
cargo run -p base-witness-diff -- \
  --local <PATH>    path to the local witness JSON file
  --rpc-url <URL>   HTTP(S) URL of the reference L2 node to compare against
  --block <NUMBER>  decimal block number to fetch from the reference node
```

`--block` is decimal here, not hex. The tool converts it internally when making the RPC call. The reference witness is fetched live at runtime; there is no need to save it separately.

Example:

```
cargo run -p base-witness-diff -- \
  --local ~/witnesses/43127830_witness.json \
  --rpc-url https://reference-node:8545 \
  --block 43127830
```

## Reading the output

The tool prints four sections.

`=== Overview ===` shows the raw counts of state trie node preimages, bytecodes, and key preimages in each witness, then for each category how many are common to both, only in the local witness, or only in the reference witness. Comparison is by keccak256 hash of the raw bytes, so a node present in both witnesses with the same content is counted as common regardless of where it appears in the trie. Any nonzero only-local or only-rpc count means the two witnesses touched a different set of trie nodes, which is the expected symptom of a divergence. It also reports how many address preimages are available, which affects whether account addresses can be resolved in later sections.

`=== Differing Leaf Nodes ===` is the core analysis. The tool decodes every RLP-encoded trie node from both witnesses, filters down to leaf nodes (which hold actual account or storage values rather than internal trie structure), and finds pairs that share the same trie path but carry different values. These are the nodes where the two executions produced a different result for the same key. For each differing leaf it attempts to decode the value as a `TrieAccount` (the RLP-encoded struct containing nonce, balance, code hash, and storage root). If both sides decode successfully, it prints the account side-by-side with a `✓` on matching fields and `DIFFERS` on mismatches. If the address can be recovered from the key preimages in the witness, it is shown; otherwise the path suffix in nibble hex is the only identifier. Leaves that do not decode as accounts are counted as storage leaf diffs — these represent individual storage slot values that diverged.

`=== Leaves Only in One Witness ===` appears only when there are trie leaves that exist in one witness but have no corresponding path entry in the other. This happens when an account is created or deleted during execution, or when a storage slot is written by one execution but not accessed at all by the other. Local-only leaves represent state that your node computed but the reference did not touch; RPC-only leaves are the inverse. Each is printed with its account data or raw storage value.

`=== SUMMARY ===` collects all counts into a single block for quick comparison. If both witnesses are fully identical the final line reads `Witnesses are identical.`

## Witness JSON format

The tool accepts witnesses in either geth or reth output format. The `state` field contains hex-encoded RLP-encoded trie node preimages (branch, extension, and leaf nodes). The `codes` field contains hex-encoded raw contract bytecodes. The `keys` field contains hex-encoded RLP-encoded key preimages, which are 20-byte addresses encoded as RLP strings (the `0x94` prefix followed by the 20 address bytes) — these are what allow the tool to resolve a trie path back to a human-readable address. The `headers` field is accepted but ignored. All fields default to empty if absent.
