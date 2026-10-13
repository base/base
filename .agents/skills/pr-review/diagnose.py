"""Plain-language explanations for the ways a local review can fail to start or to reach a service.

`just review` runs on a contributor's machine, where the `amp` CLI may be missing, signed out, or unable to
reach Amp, and where `gh` may not be signed in. Those are setup problems, not bugs, and the raw error
("Error: Cannot reach Amp servers. Are you offline?") does not say what to do about them. This module
turns a raw error into one short explanation with the next step. Edit the wording here.
"""

from __future__ import annotations

import dataclasses
import re

# What a person can do about the model service.
AMP_SETUP_HELP = (
    "The review runs its models through the `amp` CLI. Sign in with `amp login`, or set AMP_API_KEY to an "
    "access token from https://ampcode.com/settings. `amp usage` shows which account the CLI is using."
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
AMP_RULES: tuple[tuple[re.Pattern[str], Diagnosis], ...] = tuple(
    (re.compile(pattern, re.IGNORECASE), diagnosis)
    for pattern, diagnosis in (
        (r"invalid or missing api key|amp login|not logged in|unauthori[sz]ed|\b401\b",
         Diagnosis("{target} does not know who you are: the `amp` CLI has no valid sign-in.", AMP_SETUP_HELP)),
        (r"credit|billing|usage limit|quota|insufficient|\b402\b",
         Diagnosis("Your Amp account has no credit left, or has reached its usage limit.",
                   "Check `amp usage`, add credit or wait for the limit to reset, then run the review again.")),
        (r"not enabled for your account|\b403\b|forbidden|permission",
         Diagnosis("Your Amp account is not allowed to do something this review needs.",
                   "Ask whoever manages your Amp workspace, or run the review with a different `--mode`.")),
        (r"cannot reach amp servers|are you offline|econnrefused|connection refused|enotfound|getaddrinfo|"
         r"could not resolve host|dns",
         Diagnosis("Can't connect to {target}.",
                   "Check your internet connection, any VPN or proxy (`amp.proxy` in the Amp settings), and "
                   "AMP_URL if you set it.", retryable=True)),
        (r"etimedout|timed out|timeout|econnreset|socket hang up|network",
         Diagnosis("The connection to {target} timed out or was dropped.",
                   "Check your connection, VPN or proxy, and try again.", retryable=True)),
        (r"certificate|ssl|tls|self.signed|unable to verify",
         Diagnosis("The connection to {target} failed a certificate check.",
                   "If you are behind a corporate proxy, set NODE_EXTRA_CA_CERTS to its certificate file.")),
        (r"\b429\b|rate limit|rate_limit|overloaded|\b50[0-4]\b|internal server error|unexpected error inside amp",
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


def explain_amp_error(raw: str) -> Diagnosis | None:
    return explain(raw, AMP_RULES)


def explain_github_error(raw: str) -> Diagnosis | None:
    return explain(raw, GITHUB_RULES)


def amp_host(url: str | None) -> str:
    """The host to name in a message: AMP_URL's, else ampcode.com.

    Only the host is shown. A URL can carry a user name, a password or a token, and must not be printed.
    """
    if not url:
        return "Amp (ampcode.com)"
    from urllib.parse import urlsplit  # local import: only needed here

    try:
        host = urlsplit(url if "//" in url else f"//{url}").hostname
    except ValueError:
        host = None
    return host or "Amp (AMP_URL)"
