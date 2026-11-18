# `basectl-cli`

CLI parser, command implementations, and interactive monitor for Base infrastructure.

## Overview

Owns the `basectl` clap parser and all command behavior, including block, sync,
txpool, peer, proof, conductor, sequencer, batcher, and diagnostic workflows. `Cli::run`
dispatches parsed commands and returns a process outcome.

The crate also provides the interactive terminal monitor for block production,
data availability, node sync status, and system metrics.

## Denim Readiness

The upgrades monitor attaches `BaseTime` checks to Denim, using the live consensus
node's `optimism_rollupConfig` Denim timestamp. `DenimChecker` checks a hash-pinned
L2 snapshot for `BaseTime` installation, update metadata and receipt, storage/getter
agreement, 200ms cadence, and millisecond RPC fields. Active-only checks start at
Denim.

## Pods View

`basectl monitor pods` displays Kubernetes pod status from groups defined in a
local network config. Keep environment-specific names, namespaces, contexts, and
URLs in user-local config; this public crate only stores the generic schema.

```yaml
pods:
  refresh_interval_ms: 1000
  groups:
    - alias: example
      label: Example
      context: example-context
      namespace: example-namespace
```

## Leader Transfer Approval Gateway

Optional client for an approval gateway; proposals are approved by a different
operator at the review URL the gateway returns, and only then can they be
executed. When a gateway is configured, `conductor transfer-leader` and the
TUI's direct transfer actions are refused, and transfers go through
`basectl conductor transfer`. Without a gateway, behavior is unchanged.

The gateway authenticates callers and enforces every authorization and approval
rule, and it runs the transfer on the current leader. basectl only sends
requests and shows the gateway's answers verbatim.

```text
basectl conductor transfer propose --to <TARGET> --reason <TEXT>   # prints the id and review URL
# a different operator approves the proposal at the review URL
basectl conductor transfer status <ID>
basectl conductor transfer execute <ID>                            # transfers leadership now
```

| Setting | Source, in precedence order |
| ------- | --------------------------- |
| Gateway URL | `--gate-url`, `BASECTL_GATE_URL`, config `gate_url` |
| Bearer token | `--gate-token-file`, `BASECTL_GATE_TOKEN`, config `gate_token_command` (trimmed stdout) |
| Request timeout | config `gate_timeout_secs` (default 150) |

A set but invalid `BASECTL_GATE_URL` is an error, never "no gateway". The
gateway URL, from any source, must be `https://`; `http://` is accepted only
for localhost (`localhost`, `127.0.0.0/8`, `::1`, for example
`http://127.0.0.1:4001`), so the bearer token never travels in cleartext.
If `gate_token_command` runs longer than 60 s, basectl kills its shell and
fails with "token command timed out after 60s".

Setup is only `gate_url`. If the gateway's 401 response includes a login URL,
basectl opens it in the browser and receives the token on a loopback callback;
a paste prompt is the fallback.

- **Explicit sources win.** `--gate-token-file`, then `BASECTL_GATE_TOKEN`,
  then `gate_token_command`. They never trigger the browser, the prompt, or the
  cache. In a one-shot command a 401 for them just fails; the TUI's cluster
  view reads the source again (see **TUI** below).
- **Browser login.** With no explicit source and no usable cached token, the
  request is sent without a token. The gateway's 401 body may carry a
  `token_url` (`https://`, or `http://` on loopback). basectl then listens once
  on `127.0.0.1:<random port>`, opens
  `<token_url>?callback_url=http://127.0.0.1:<port>/callback?state=<state>`
  (URL-encoded) with `open` (macOS) or `xdg-open` (Linux), and prints the URL.
  The login page posts `{"token", "expiresAt"}` to the callback URL. Each login
  gets a random 128-bit `state`, and the listener answers any callback without
  the matching `state` with 403 and keeps waiting, so only a page opened from
  the gateway-provided login URL can hand basectl a bearer token for the
  approval gateway. The listener answers CORS preflight
  for the login page's origin only, caps the body at 64 KB, serves each
  connection separately with a 5 s limit (so an idle connection can't block
  the callback), and gives up after 120 s. The original request is then retried exactly once with the new
  token. A 401 means the gateway did nothing, so the retry is safe for execute
  too.
- **Fallbacks.** If there is no `token_url`, the browser cannot be opened, or
  the callback times out, an interactive terminal gets a no-echo
  `Paste token:` prompt. Otherwise the command fails with the login URL (if
  any) and the explicit options.
- **Cache.** Tokens are cached as JSON `{"token", "expiresAt"}` at
  `${XDG_CACHE_HOME:-~/.cache}/basectl/gate-token-<host>[_<port>]`, in a
  `0700` directory with a `0600` file written atomically. A looser file is
  tightened before use, and older bare-token caches are still read. A cached
  token is reused until 5 minutes before `expiresAt` (ISO 8601, or epoch
  seconds or milliseconds), or its JWT `exp` when absent, or until a 401. A
  new token is used at once and the cache is written in the background; a
  one-shot command waits up to 2 s for that write before it exits. A failed
  or abandoned write logs a warning, leaves the previous cache file intact,
  and costs one more sign-in next run.
- **TUI.** The TUI never prompts. Its proposal actions ask you to run
  `basectl conductor transfer status <ID>` once in a shell to log in, or to
  set an explicit token source. In gateway mode (below), a 401 on the cluster
  view pauses polling. Without an explicit source it starts one browser login,
  and polling resumes when a token is available (from that login or the
  cache); a failed login is not retried for 5 minutes. With an explicit
  source it reads that source again instead, at most once per 5 minutes,
  never falling back to the browser, the prompt, or the cache. A source that
  returns the rejected token waits for the next window, so the gateway sees
  at most one request per window with it.

The gateway URL is a base URL; the client appends the paths below. Every
request sends `Authorization: Bearer <token>`.

- `POST /admin/conductor/proposals` with `{"target_id", "justification"}`:
  returns `{"id", "review_url"}`
- `GET /admin/conductor/proposals/{id}`: review status, approvers, and the
  transfer outcome
- `POST /admin/conductor/proposals/{id}/execute` (no body): transfers
  leadership for an approved proposal, returning `{"id", "outcome", "detail"}`
  with outcome `SUCCEEDED` (200) or `FAILED`/`UNKNOWN` (502)
- `GET /admin/conductor/cluster`: read-only cluster view (leader, membership
  version, and per node: role, conductor and sequencer state, heads (plus the
  unsafe hash, `current_l1` and `head_l1`), `cl_peers`, and an `error` when
  the gateway could not read it; fields an older gateway omits read as unknown), sent with a 3 s timeout and
  never logged in to automatically

Errors are JSON `{"error": "<reason>"}` and are shown with their HTTP status.
Gateway error text (or a non-JSON body) is shown with ANSI escapes and control
characters stripped, capped at 512 characters.

- A 503 (for example while leadership is changing) is retried automatically up
  to 3 times, about 1 s apart, for `POST /admin/conductor/proposals` and
  `GET /admin/conductor/proposals/{id}`. It is never retried for
  `POST /admin/conductor/proposals/{id}/execute`: nothing was transferred, so
  run execute again.
- On execute, a 504, a 502 without the gateway's JSON outcome, a 2xx whose
  body isn't a valid execute response, a client timeout, or a connection lost
  after sending is reported as `UNKNOWN`, because
  the transfer may already have happened; check `conductor_leaderWithID` before
  retrying.
- A 412 (target failed its pre-transfer check) transfers nothing; execute can
  be retried once the target is healthy and the approval is still valid.

### Gateway mode in the TUI

When a gateway is set and no conductor RPC is configured (no `conductors:`
list, no `--conductor-rpc`, no discovery bootstrap), the conductor view reads
its nodes from `GET /admin/conductor/cluster` every 2 s instead of polling each
node's RPCs every 200 ms. The header says "via approval gateway". It shows
role, conductor and sequencer state, heads, the unsafe hash, L1 derivation
("L1 Derived") and CL peers when the gateway serves them (`?` when it
doesn't), and each node's gateway error, and hides the safe hash and EL rows.
`t` and "Transfer leader here" propose a transfer to the node's raft server ID; actions that need direct conductor RPC
access (pause, resume, sequencer start and stop, P2P, restart) explain that
they are unavailable. In the proposals list, one execute runs at a time: its
row shows "executing…" and `x` does nothing until it finishes. With conductor
RPC configured, the view is unchanged.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
basectl-cli = { workspace = true }
```

```rust,ignore
use basectl_cli::Cli;
use clap::Parser;

if Cli::parse().run().await?.has_failures() {
    std::process::exit(1);
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
