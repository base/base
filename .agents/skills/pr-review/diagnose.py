"""Plain-language explanations for the ways a local review can fail to start or to reach a service.

`just review` runs on a contributor's machine, where the LLM gateway may not be configured or reachable, and
where `gh` may not be signed in. Those are setup problems, not bugs, and the raw error ("<urlopen error [Errno
111] Connection refused>", or just an HTTP status code) does not say what to do about them. This module turns a
raw error into one short explanation with the next step. Edit the wording here.

Each stage runs one Claude Code (`claude -p`) process, pointed at `https://$LLM_GATEWAY_HOSTNAME` with the key
in LLM_GATEWAY_API_KEY, limited to 12 turns and a wall-clock timeout, and never restarted. The raw error is the
CLI's reported failure or the script's own timeout message, so patterns cover CLI wording as well as HTTP status
codes and network errors that the CLI passes through.
"""

from __future__ import annotations

import dataclasses
import re
from urllib.parse import urlsplit

# What a person can do about the model service. There is no other provider to fall back to.
MODEL_SETUP_HELP = (
    "The review calls models only through the LLM gateway. Set LLM_GATEWAY_HOSTNAME to the gateway's host name "
    "(optionally with :port, not a URL) and LLM_GATEWAY_API_KEY to a key for it."
)
GITHUB_SETUP_HELP = (
    "Pull request reviews read the pull request through the `gh` CLI. Sign in with `gh auth login`, "
    "or set GH_TOKEN to a token that can read the repository."
)
# How a message names the gateway when its host name cannot be read from the setting.
GATEWAY_FALLBACK_NAME = "the LLM gateway (LLM_GATEWAY_HOSTNAME)"


@dataclasses.dataclass(frozen=True)
class Diagnosis:
    """What went wrong, in a sentence, and what to do next."""

    summary: str
    next_step: str
    # Whether running the review again can help. A missing key cannot be fixed by waiting; an overloaded
    # gateway can. The review never retries by itself: a person decides to run it again.
    retryable: bool = False

    def text(self, target: str) -> str:
        """The explanation for a service, `target` being its host name (it replaces `{target}`)."""
        return f"{self.summary.replace('{target}', target)}\n  {self.next_step}"


# (pattern in the raw error, how to explain it). Order matters: the first match wins. The stage limits come first,
# because a CLI failure can quote other text (a status code, "network") that would otherwise match a later rule.
MODEL_RULES: tuple[tuple[re.Pattern[str], Diagnosis], ...] = tuple(
    (re.compile(pattern, re.IGNORECASE), diagnosis)
    for pattern, diagnosis in (
        (r"error_max_turns|max(imum)?[ _-]turns",
         Diagnosis("A review stage using {target} used all of its turns without giving a final answer.",
                   "Each stage has at most 12 turns of Read, Grep, and Glob. The stage is not restarted; run the "
                   "review again, or narrow the agent's prompt if it keeps exploring too long.",
                   retryable=True)),
        (r"`claude` timed out after",
         Diagnosis("A review stage using {target} did not finish within its wall-clock limit.",
                   "Each stage's Claude Code process is stopped after 600 s by default (`timeout_seconds` in the "
                   "agent file). The stage is not restarted; run the review again, or lower `effort`.",
                   retryable=True)),
        (r"not logged in|please run /login|missing api key|no api key|x-api-key header is required",
         Diagnosis("The request to {target} is not signed in: it carried no usable gateway key.",
                   MODEL_SETUP_HELP)),
        (r"invalid api key|invalid x-api-key|authentication_error|\b401\b",
         Diagnosis("{target} rejected your credentials.",
                   "Check that LLM_GATEWAY_API_KEY is correct and has not expired, and that LLM_GATEWAY_HOSTNAME "
                   "is the gateway the key belongs to.")),
        (r"credit balance|billing|quota|insufficient",
         Diagnosis("The account behind your key has no usable credit or quota on {target}.",
                   "Add credit or use another key; then run the review again.")),
        (r"restricted model|not allowed to use|permission_error|does not have permission",
         Diagnosis("{target} will not let your key use one of the models this review is set up to use.",
                   "Ask whoever runs your gateway to allow it, or use a model you can use for every stage: "
                   "`just review --model claude-sonnet-5-5`.")),
        (r"\b403\b",
         Diagnosis("The request to {target} was denied (HTTP 403).",
                   "Check gateway or proxy access logs: this status alone cannot distinguish a blocked client "
                   "or network from a key, endpoint, or model permission problem.")),
        (r"issue with the selected model|invalid model name|model_not_found|not_found_error|\b404\b",
         Diagnosis("A model this review is set up to use does not exist on {target}.",
                   "Use one you have for every stage with `--model <name>`, for example "
                   "`just review --model claude-sonnet-5-5`. The agents' own models are in "
                   ".agents/skills/pr-review/agents/.")),
        (r"econnrefused|connection refused",
         Diagnosis("Can't connect to {target}: the connection was refused.",
                   "Check that LLM_GATEWAY_HOSTNAME (and its port, if you set one) is right, that any proxy "
                   "allows it, and that the gateway is running.")),
        (r"enotfound|getaddrinfo|dns|can't reach the api server|could not resolve host|name or service not known"
         r"|nodename nor servname|temporary failure in name resolution|no address associated",
         Diagnosis("Can't find {target}: its address does not resolve.",
                   "Check your internet connection, any VPN, and that LLM_GATEWAY_HOSTNAME is spelled correctly.")),
        (r"etimedout|timed out|timeout|econnreset|connection reset|socket hang up|remote end closed|network",
         Diagnosis("The connection to {target} timed out or was dropped.",
                   "Check your connection, VPN or proxy. The review does not restart the stage; run it again.",
                   retryable=True)),
        (r"certificate|ssl|tls|self.signed|unable to verify",
         Diagnosis("The connection to {target} failed a certificate check.",
                   "If you are behind a corporate proxy, set NODE_EXTRA_CA_CERTS to a bundle that includes its "
                   "certificate.")),
        (r"\b429\b|rate limit|rate_limit|overloaded|\b529\b|\b50[0-4]\b|internal server error|server-side",
         Diagnosis("{target} is busy or having a problem on its side.",
                   "This is usually temporary. The review does not restart the stage; wait a minute and run it "
                   "again.",
                   retryable=True)),
        (r"\b400\b|\b413\b|invalid_request_error|request_too_large|prompt is too long|context.length"
         r"|max_tokens|max_output_tokens",
         Diagnosis("{target} rejected a request as invalid for its model.",
                   "Usual causes: `max_output_tokens` above the model's limit (Gemini models are set to 32000), "
                   "a prompt longer than the model's context window, or a model that does not accept `effort`. "
                   "The prompt is kept as `<agent>.prompt.md` with `--artifacts-dir`.")),
    )
)

GITHUB_RULES: tuple[tuple[re.Pattern[str], Diagnosis], ...] = tuple(
    (re.compile(pattern, re.IGNORECASE), diagnosis)
    for pattern, diagnosis in (
        (r"bad credentials|http 401",
         Diagnosis("{target} rejected your token.", GITHUB_SETUP_HELP)),
        (r"gh auth login|to get started with github cli|no oauth token",
         Diagnosis("The `gh` CLI is not signed in to {target}.", GITHUB_SETUP_HELP)),
        (r"could not resolve to a pullrequest",
         Diagnosis("That pull request does not exist in this repository.",
                   "Check the number, or pass the full URL, or use --repo OWNER/REPO.")),
        (r"could not resolve to a repository|http 404|not found",
         Diagnosis("{target} cannot find that repository, or your token cannot see it.",
                   "Check the name, and that your `gh` account can read it (a private repository needs access).")),
        (r"rate limit|http 403|http 429|abuse",
         Diagnosis("{target} is limiting your requests.",
                   "Wait a few minutes, or sign in with `gh auth login` for a higher limit.", retryable=True)),
        (r"error connecting|check your internet|no such host|connection refused|timeout|timed out",
         Diagnosis("Can't connect to {target}.", "Check your internet connection, VPN or proxy, and try again.",
                   retryable=True)),
    )
)


def explain(raw: str, rules: tuple[tuple[re.Pattern[str], Diagnosis], ...]) -> Diagnosis | None:
    """The first rule that matches the raw error, or None if the error is not a known kind."""
    for pattern, diagnosis in rules:
        if pattern.search(raw):
            return diagnosis
    return None


def explain_model_error(raw: str) -> Diagnosis | None:
    return explain(raw, MODEL_RULES)


def explain_github_error(raw: str) -> Diagnosis | None:
    return explain(raw, GITHUB_RULES)


def model_service_host(base_url: str | None) -> str:
    """The host to name in a message: the gateway's host name, from a URL or a bare LLM_GATEWAY_HOSTNAME value.

    Only the host is shown, never a port, path, user name, password, or token. With nothing configured it says
    so; there is no default provider to name.
    """
    if not base_url:
        return "the LLM gateway (LLM_GATEWAY_HOSTNAME is not set)"
    try:
        host = urlsplit(base_url if "//" in base_url else f"//{base_url}").hostname
    except ValueError:
        host = None
    return host or GATEWAY_FALLBACK_NAME
