"""Plain-language explanations for the ways a local review can fail to start or to reach a service.

`just review` runs on a contributor's machine, where the `claude` CLI may be missing, signed out, or unable to
reach the model gateway, and where `gh` may not be signed in. Those are setup problems, not bugs, and the raw
error ("API Error: Connection refused ... (ECONNREFUSED)") does not say what to do about them. This module
turns a raw error into one short explanation with the next step. Edit the wording here.
"""

from __future__ import annotations

import dataclasses
import re

# What a person can do about the model service. The default points at the open-source route.
MODEL_SETUP_HELP = (
    "The review calls models through the `claude` CLI. Sign in with `claude auth login`, or set "
    "ANTHROPIC_API_KEY (and ANTHROPIC_BASE_URL if you use a gateway or proxy). "
    "Run `claude auth status` to see what the CLI is using."
)
GITHUB_SETUP_HELP = (
    "Pull request reviews read the pull request through the `gh` CLI. Sign in with `gh auth login`, "
    "or set GH_TOKEN to a token that can read the repository."
)


@dataclasses.dataclass(frozen=True)
class Diagnosis:
    """What went wrong, in a sentence, and what to do next."""

    summary: str
    next_step: str
    # Whether trying again can help. A missing sign-in cannot be fixed by waiting; an overloaded API can.
    retryable: bool = False

    def text(self, target: str) -> str:
        """The explanation for a service, `target` being its host name (it replaces `{target}`)."""
        return f"{self.summary.replace('{target}', target)}\n  {self.next_step}"


# (pattern in the raw error, how to explain it). Order matters: the first match wins.
MODEL_RULES: tuple[tuple[re.Pattern[str], Diagnosis], ...] = tuple(
    (re.compile(pattern, re.IGNORECASE), diagnosis)
    for pattern, diagnosis in (
        (r"not logged in|please run /login",
         Diagnosis("The `claude` CLI is not signed in, so it cannot use {target}.", MODEL_SETUP_HELP)),
        (r"invalid api key|invalid x-api-key|authentication_error|\b401\b",
         Diagnosis("{target} rejected your credentials.",
                   "Check that ANTHROPIC_API_KEY is correct and has not expired, and that ANTHROPIC_BASE_URL "
                   "(if set) is the service the key belongs to.")),
        (r"credit balance|billing|quota|insufficient",
         Diagnosis("The account behind your key has no usable credit or quota on {target}.",
                   "Add credit or use another key; then run the review again.")),
        (r"restricted model|not allowed to use|permission_error|does not have permission|\b403\b",
         Diagnosis("{target} will not let your key use one of the models this review is set up to use.",
                   "Ask whoever runs your gateway to allow it, or use a model you can use for every stage: "
                   "`just review --model sonnet`.")),
        (r"issue with the selected model|invalid model name|model_not_found|not_found_error|\b404\b",
         Diagnosis("A model this review is set up to use does not exist on {target}.",
                   "Use one you have for every stage with `--model <name>`, for example "
                   "`just review --model sonnet`. The agents' own models are in .agents/skills/pr-review/agents/.")),
        (r"econnrefused|connection refused",
         Diagnosis("Can't connect to {target}: the connection was refused.",
                   "Check that ANTHROPIC_BASE_URL (and any proxy) is right and that the service is running. "
                   "`claude auth status` shows what the CLI is using.")),
        (r"enotfound|getaddrinfo|dns|can't reach the api server|could not resolve host",
         Diagnosis("Can't find {target}: its address does not resolve.",
                   "Check your internet connection, any VPN, and that ANTHROPIC_BASE_URL (if set) is spelled "
                   "correctly.")),
        (r"etimedout|timed out|timeout|econnreset|socket hang up|network",
         Diagnosis("The connection to {target} timed out or was dropped.",
                   "Check your connection, VPN or proxy, and try again.", retryable=True)),
        (r"certificate|ssl|tls|self.signed|unable to verify",
         Diagnosis("The connection to {target} failed a certificate check.",
                   "If you are behind a corporate proxy, set NODE_EXTRA_CA_CERTS to its certificate file.")),
        (r"\b429\b|rate limit|rate_limit|overloaded|\b529\b|\b50[0-4]\b|internal server error|server-side",
         Diagnosis("{target} is busy or having a problem on its side.",
                   "This is usually temporary. Wait a minute and run the review again.", retryable=True)),
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
    """The host to name in a message: the gateway from ANTHROPIC_BASE_URL, else Anthropic's API.

    Only the host is shown. A URL can carry a user name, a password or a token, and must not be printed.
    """
    if not base_url:
        return "the Anthropic API (api.anthropic.com)"
    from urllib.parse import urlsplit  # local import: only needed here

    try:
        host = urlsplit(base_url if "//" in base_url else f"//{base_url}").hostname
    except ValueError:
        host = None
    return host or "the model service (ANTHROPIC_BASE_URL)"
