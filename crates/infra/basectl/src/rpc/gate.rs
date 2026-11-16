//! Client for an optional leader-transfer approval gateway.
//!
//! The gateway, not basectl, authenticates callers and enforces the approval
//! rules: a proposal is approved by a different operator at the review URL the
//! gateway returns, and only then does execute transfer leadership.

use std::{
    collections::BTreeMap,
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use alloy_transport_http::reqwest::{self, Method, StatusCode};
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{process::Command, sync::oneshot};
use tracing::warn;
use url::{Host, Url};

use crate::{
    BrowserLogin, ClusterResponse, LoginListener, MonitoringConfig, NoPrompt, TerminalPrompter,
    TokenCache, TokenPrompter,
};

/// Error returned by the approval gateway client.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum GateError {
    /// No bearer token could be resolved; no request was sent.
    #[error(
        "no approval gateway token: run a `basectl conductor transfer` command in a terminal to be prompted, pass --gate-token-file, set BASECTL_GATE_TOKEN, or configure `gate_token_command`"
    )]
    MissingToken,
    /// A login is needed but no browser login or prompt was possible.
    #[error(
        "approval gateway login required{}; or pass --gate-token-file, set BASECTL_GATE_TOKEN, or configure `gate_token_command`",
        .url.as_deref().map(|url| format!(": open {}", url.escape_debug())).unwrap_or_default()
    )]
    LoginRequired {
        /// Login URL, when the gateway provided one.
        url: Option<String>,
    },
    /// The gateway rejected the token (401); it did nothing.
    #[error(
        "approval gateway returned 401: {message}{}",
        .token_url.as_deref().map(|url| format!("; get a token at {}", url.escape_debug())).unwrap_or_default()
    )]
    Unauthenticated {
        /// The gateway's `error` text, verbatim.
        message: String,
        /// Where to obtain a token, when the gateway says.
        token_url: Option<String>,
    },
    /// Reading the token file or running the token command failed.
    #[error("failed to obtain approval gateway token: {0}")]
    Token(String),
    /// The gateway answered with an error status.
    #[error("approval gateway returned {status}: {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// The gateway's `error` text, verbatim.
        message: String,
    },
    /// The request could not be sent.
    #[error("approval gateway request failed: {0}")]
    Transport(String),
    /// The request may have reached the gateway, but no response was read
    /// (timeout or connection lost mid-request).
    #[error("approval gateway request interrupted: {0}")]
    Interrupted(String),
    /// The proposal ID is not 1-128 of `[A-Za-z0-9_-]`; no request was sent.
    #[error("invalid proposal id")]
    InvalidProposalId,
    /// The sanitized reason was empty; no request was sent.
    #[error("reason required")]
    ReasonRequired,
    /// `BASECTL_GATE_URL` is set but doesn't parse; no request was sent.
    #[error("BASECTL_GATE_URL is not a valid URL: {0}")]
    InvalidEnvUrl(String),
    /// The gateway URL isn't `https://` (or `http://` on loopback), so the
    /// bearer token would travel in cleartext; no request was sent.
    #[error("gate_url must be https:// (http:// only for localhost): {0}")]
    InsecureUrl(String),
}

impl GateError {
    /// Message for the TUI, which cannot prompt: token problems point to the CLI.
    pub fn tui_message(&self) -> String {
        match self {
            Self::MissingToken | Self::Unauthenticated { .. } | Self::LoginRequired { .. } => {
                "Approval gateway login needed: run `basectl conductor transfer status <ID>` once in a shell to log in, or set an explicit token source.".to_string()
            }
            other => other.to_string(),
        }
    }
}

impl From<reqwest::Error> for GateError {
    fn from(error: reqwest::Error) -> Self {
        let message = GateClient::sanitize_message(&error.to_string());
        if error.is_connect() || error.is_builder() {
            Self::Transport(message)
        } else {
            Self::Interrupted(message)
        }
    }
}

/// Where to find the gateway and its bearer token.
#[derive(Clone, PartialEq, Eq)]
pub struct GateSettings {
    /// Gateway base URL.
    pub url: Url,
    /// File holding the bearer token.
    pub token_file: Option<PathBuf>,
    /// Shell command whose trimmed stdout is the bearer token.
    pub token_command: Option<String>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Token from `BASECTL_GATE_TOKEN`, captured by [`Self::resolve`].
    pub env_token: Option<String>,
    /// Root of the token cache; `None` disables caching.
    pub cache_root: Option<PathBuf>,
    /// How long basectl waits for `token_command` before killing its shell
    /// and giving up.
    pub token_command_timeout: Duration,
}

impl fmt::Debug for GateSettings {
    /// Shows whether `BASECTL_GATE_TOKEN` was set, never its value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GateSettings")
            .field("url", &self.url)
            .field("token_file", &self.token_file)
            .field("has_token_command", &self.token_command.is_some())
            .field("timeout", &self.timeout)
            .field("has_env_token", &self.env_token.is_some())
            .field("cache_root", &self.cache_root)
            .field("token_command_timeout", &self.token_command_timeout)
            .finish_non_exhaustive()
    }
}

impl GateSettings {
    /// Environment variable overriding the gateway URL.
    pub const URL_ENV: &str = "BASECTL_GATE_URL";
    /// Environment variable holding the bearer token.
    pub const TOKEN_ENV: &str = "BASECTL_GATE_TOKEN";
    /// Default per-request timeout. Longer than the gateway's worst-case
    /// execute, so the gateway reports the outcome, not the client.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(150);
    /// Default bound on `gate_token_command`, so a hung helper can't hang
    /// basectl.
    pub const TOKEN_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

    /// Whether a bearer token may be sent to `url`: `https://`, or `http://`
    /// only on a loopback host (`localhost`, `127.0.0.0/8`, `::1`).
    pub fn is_secure_or_loopback(url: &Url) -> bool {
        match url.scheme() {
            "https" => true,
            "http" => match url.host() {
                Some(Host::Domain(domain)) => domain == "localhost",
                Some(Host::Ipv4(ip)) => ip.is_loopback(),
                Some(Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            },
            _ => false,
        }
    }

    /// Resolves settings from the flag, then `BASECTL_GATE_URL`, then config.
    /// Returns `Ok(None)` when no gateway is configured. A set but invalid
    /// `BASECTL_GATE_URL` is an error rather than "no gateway", so a typo
    /// can't silently turn the gateway off. (The flag and config are parsed
    /// as URLs when they are read, so they fail there.) The resolved URL,
    /// whatever its source, must pass [`Self::is_secure_or_loopback`].
    pub fn resolve(
        config: &MonitoringConfig,
        url_flag: Option<Url>,
        token_file: Option<PathBuf>,
    ) -> Result<Option<Self>, GateError> {
        Self::resolve_with_env(config, url_flag, token_file, |key| std::env::var(key).ok())
    }

    /// [`Self::resolve`] with environment lookups through `env`. An empty
    /// `BASECTL_GATE_URL` or `BASECTL_GATE_TOKEN` counts as unset.
    pub fn resolve_with_env(
        config: &MonitoringConfig,
        url_flag: Option<Url>,
        token_file: Option<PathBuf>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, GateError> {
        let env_url = env(Self::URL_ENV)
            .filter(|raw| !raw.trim().is_empty())
            .map(|raw| {
                Url::parse(raw.trim()).map_err(|error| {
                    GateError::InvalidEnvUrl(format!("\"{}\": {error}", raw.escape_debug()))
                })
            })
            .transpose()?;
        let Some(url) = url_flag.or(env_url).or_else(|| config.gate_url.clone()) else {
            return Ok(None);
        };
        if !Self::is_secure_or_loopback(&url) {
            // The URL is shown without credentials or query, either of which
            // could carry a secret.
            let mut shown = url;
            let _ = shown.set_username("");
            let _ = shown.set_password(None);
            shown.set_query(None);
            return Err(GateError::InsecureUrl(shown.as_str().escape_debug().to_string()));
        }
        let timeout = config.gate_timeout_secs.map_or(Self::DEFAULT_TIMEOUT, Duration::from_secs);
        Ok(Some(Self {
            url,
            token_file,
            token_command: config.gate_token_command.clone(),
            timeout,
            env_token: env(Self::TOKEN_ENV).filter(|token| !token.trim().is_empty()),
            cache_root: TokenCache::default_root(),
            token_command_timeout: Self::TOKEN_COMMAND_TIMEOUT,
        }))
    }

    /// Whether a gateway is configured. A set but invalid `BASECTL_GATE_URL`
    /// counts as configured, so direct transfers stay refused.
    pub fn configured(config: &MonitoringConfig) -> bool {
        !matches!(Self::resolve(config, None, None), Ok(None))
    }

    /// Builds a client that prompts on the terminal when it needs a token.
    pub async fn client(&self) -> Result<GateClient, GateError> {
        self.client_with_prompter(Arc::new(TerminalPrompter)).await
    }

    /// Builds a client that never prompts (for the TUI).
    pub async fn client_without_prompt(&self) -> Result<GateClient, GateError> {
        self.client_with_prompter(Arc::new(NoPrompt)).await
    }

    /// Builds a client for the TUI's cluster view: a login may open the
    /// browser, but nothing is printed to the terminal and nothing prompts.
    pub async fn client_for_tui_login(&self) -> Result<GateClient, GateError> {
        let mut client = self.client_with_prompter(Arc::new(BrowserLogin)).await?;
        client.announce_login = false;
        Ok(client)
    }

    /// Resolves the token and builds a client. Explicit sources win: the token
    /// file, then `BASECTL_GATE_TOKEN`, then `gate_token_command`. Otherwise
    /// the cached token is used, or `prompter` asks for one and it is cached.
    /// Fails before any request when no token is available.
    pub async fn client_with_prompter(
        &self,
        prompter: Arc<dyn TokenPrompter>,
    ) -> Result<GateClient, GateError> {
        let (token, cache, explicit) = match self.explicit_token().await? {
            Some(token) => (Some(token), None, Some(Arc::new(self.clone()))),
            None => {
                let cache = self
                    .cache_root
                    .as_deref()
                    .map(|root| TokenCache::for_gate(root, &self.url))
                    .ok_or(GateError::MissingToken)?;
                // With no usable cached token, the first request goes out
                // unauthenticated so the gateway's 401 can say how to log in.
                (cache.load(Utc::now()), Some(cache), None)
            }
        };
        // Never follow redirects: requests carry a bearer token, execute is a
        // non-idempotent POST, and only the configured base URL is trusted.
        let http = reqwest::Client::builder()
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(GateClient {
            base: self.url.clone(),
            token: Arc::new(Mutex::new(token)),
            cache,
            explicit,
            cache_writes: Arc::default(),
            prompter,
            http,
            retry_backoff: GateClient::RETRY_BACKOFF,
            login_timeout: LoginListener::DEFAULT_TIMEOUT,
            announce_login: true,
        })
    }

    async fn explicit_token(&self) -> Result<Option<String>, GateError> {
        let token = if let Some(path) = &self.token_file {
            tokio::fs::read_to_string(path)
                .await
                .map_err(|error| GateError::Token(format!("{}: {error}", path.display())))?
        } else if let Some(token) = &self.env_token {
            token.clone()
        } else if let Some(command) = &self.token_command {
            // On timeout the dropped `sh` child is killed (kill_on_drop). A
            // command that `sh` forked rather than exec'd can outlive it; it
            // isn't put in its own process group, because a helper that
            // prompts on the terminal would then be stopped (SIGTTIN).
            let output = Command::new("sh").arg("-c").arg(command).kill_on_drop(true).output();
            let output = tokio::time::timeout(self.token_command_timeout, output)
                .await
                .map_err(|_| {
                    GateError::Token(format!(
                        "token command timed out after {:?}",
                        self.token_command_timeout
                    ))
                })?
                .map_err(|error| GateError::Token(error.to_string()))?;
            if !output.status.success() {
                return Err(GateError::Token(format!(
                    "token command exited with {}",
                    output.status
                )));
            }
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            return Ok(None);
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(GateError::MissingToken);
        }
        Ok(Some(token))
    }
}

/// Serde helpers that tolerate the gateway's null and zero values: `null`
/// strings and lists become empty, and zero or unparsable times are absent.
#[derive(Debug, Clone, Copy)]
pub struct Lenient;

impl Lenient {
    /// `null` becomes the type's default (empty string, empty list).
    pub fn or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: Default + Deserialize<'de>,
    {
        Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
    }

    /// `null`, empty, unparsable, or zero (year 1 or earlier) times are `None`.
    pub fn time<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
        Ok(raw
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|at| at.with_timezone(&Utc))
            .filter(|at| at.year() > 1))
    }

    /// `null` or empty strings are `None`.
    pub fn non_empty<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Option::<String>::deserialize(deserializer)?.filter(|text| !text.is_empty()))
    }

    /// An approval with no approvers and no time (the gateway's zero value)
    /// is treated as absent.
    pub fn approval<'de, D>(deserializer: D) -> Result<Option<ReviewApproval>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Option::<ReviewApproval>::deserialize(deserializer)?
            .filter(|approval| !approval.approvers.is_empty() || approval.approved_at.is_some()))
    }
}

/// Response to a proposal request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposeResponse {
    /// Proposal identifier.
    pub id: String,
    /// Where a different operator approves the proposal.
    #[serde(
        default,
        deserialize_with = "Lenient::non_empty",
        skip_serializing_if = "Option::is_none"
    )]
    pub review_url: Option<String>,
}

impl ProposeResponse {
    /// What the proposer does next, with gateway strings escaped.
    pub fn next_steps(&self) -> String {
        let id = self.id.escape_debug();
        let execute = format!("then run `basectl conductor transfer execute {id}`");
        self.review_url.as_ref().map_or_else(
            || {
                format!(
                    "ask a different engineer to approve proposal {id} in the approval portal, {execute}"
                )
            },
            |url| format!("ask a different engineer to approve it at {}, {execute}", url.escape_debug()),
        )
    }
}

/// Leader transfer described by a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferAction {
    /// Action kind.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub action: String,
    /// Target raft server ID.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub target_id: String,
    /// Proposer's justification.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub justification: String,
    /// Network the gateway serves.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub network: String,
    /// Leader when the proposal was made.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub from_leader_id: String,
}

/// Approval recorded on the review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewApproval {
    /// Approving operators.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub approvers: Vec<String>,
    /// When the review was approved.
    #[serde(default, deserialize_with = "Lenient::time", skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<DateTime<Utc>>,
}

/// Transfer outcome recorded by the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferOutcome {
    /// Operator who ran execute.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub caller: String,
    /// `SUCCEEDED`, `FAILED`, `UNKNOWN`, or empty while the transfer runs.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub outcome: String,
    /// Gateway-provided detail.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub detail: String,
    /// When the outcome was recorded.
    #[serde(default, deserialize_with = "Lenient::time", skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl TransferOutcome {
    /// The outcome, or "in progress" while the transfer runs.
    pub fn label(&self) -> &str {
        if self.outcome.is_empty() { "in progress" } else { &self.outcome }
    }
}

/// Proposal status returned by the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalStatusResponse {
    /// Proposal identifier.
    pub id: String,
    /// Review status, shown verbatim (for example `approved`); compared case-insensitively.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub status: String,
    /// Review title.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub title: String,
    /// Proposer.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub author: String,
    /// When the proposal was created.
    #[serde(default, deserialize_with = "Lenient::time", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// The proposed transfer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<TransferAction>,
    /// Approval, once given.
    #[serde(
        default,
        deserialize_with = "Lenient::approval",
        skip_serializing_if = "Option::is_none"
    )]
    pub approval: Option<ReviewApproval>,
    /// Transfer outcome, once execute has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TransferOutcome>,
    /// Where a different operator approves the proposal.
    #[serde(
        default,
        deserialize_with = "Lenient::non_empty",
        skip_serializing_if = "Option::is_none"
    )]
    pub review_url: Option<String>,
}

impl ProposalStatusResponse {
    /// Review status the gateway requires before execute (compared
    /// case-insensitively).
    pub const APPROVED: &str = "approved";

    /// Whether the review status is approved, ignoring case.
    pub fn is_approved(&self) -> bool {
        self.status.eq_ignore_ascii_case(Self::APPROVED)
    }

    /// Warning shown before execute when the review isn't approved.
    pub fn not_approved_warning(&self) -> Option<String> {
        (!self.is_approved()).then(|| {
            format!(
                "warning: status is {}; the gate will likely refuse",
                self.status.escape_debug()
            )
        })
    }

    /// Human-readable lines with gateway strings escaped for the terminal.
    pub fn lines(&self, now: DateTime<Utc>) -> Vec<String> {
        let mut lines = vec![
            format!("proposal  {}", self.id.escape_debug()),
            format!("status    {}", self.status.escape_debug()),
            format!("title     {}", self.title.escape_debug()),
        ];
        let created = self.created_at.map_or_else(String::new, |at| {
            let age = (now - at).num_seconds().max(0);
            format!("  created {}  age {}m{:02}s", at.to_rfc3339(), age / 60, age % 60)
        });
        lines.push(format!("author    {}{created}", self.author.escape_debug()));
        if let Some(action) = &self.action {
            lines.push(format!(
                "transfer  {} -> {} on {}",
                action.from_leader_id.escape_debug(),
                action.target_id.escape_debug(),
                action.network.escape_debug()
            ));
            lines.push(format!("reason    {}", action.justification.escape_debug()));
        }
        if let Some(approval) = &self.approval {
            let at = approval.approved_at.map(|at| format!(" at {}", at.to_rfc3339()));
            lines.push(format!(
                "approved  by {}{}",
                approval.approvers.join(", ").escape_debug(),
                at.unwrap_or_default()
            ));
        }
        if let Some(outcome) = &self.outcome {
            lines.push(format!(
                "outcome   {} {}",
                outcome.label().escape_debug(),
                outcome.detail.escape_debug()
            ));
        }
        if let Some(url) = &self.review_url {
            lines.push(format!("review    {}", url.escape_debug()));
        }
        lines
    }

    /// Client-side hint for why execute looks unavailable, or `None`.
    /// The gateway decides; this only explains the visible state.
    pub fn execute_hint(&self) -> Option<String> {
        if let Some(outcome) = &self.outcome {
            return Some(format!("Already executed: {}", outcome.label().escape_debug()));
        }
        if !self.is_approved() {
            let at = self.review_url.as_deref().unwrap_or("the review URL");
            return Some(format!(
                "Not approved yet (status {}); approve at {}",
                self.status.escape_debug(),
                at.escape_debug()
            ));
        }
        let approvers = self.approval.as_ref().map_or(&[][..], |a| a.approvers.as_slice());
        if !approvers.is_empty() && approvers.iter().all(|a| a.eq_ignore_ascii_case(&self.author)) {
            return Some("Self-approval does not count; a different operator must approve".into());
        }
        None
    }
}

/// Transfer outcome as reported by execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum GateOutcomeKind {
    /// The transfer succeeded.
    Succeeded,
    /// The transfer failed and leadership did not change.
    Failed,
    /// The gateway could not tell whether leadership moved.
    Unknown,
}

/// Response to an execute request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteResponse {
    /// Proposal identifier.
    pub id: String,
    /// Transfer outcome.
    pub outcome: GateOutcomeKind,
    /// Gateway-provided detail.
    #[serde(default)]
    pub detail: String,
}

impl ExecuteResponse {
    /// One-line outcome; UNKNOWN adds how to check before retrying.
    pub fn message(&self) -> String {
        let mut line = format!(
            "{}: {:?} {}",
            self.id.escape_debug(),
            self.outcome,
            self.detail.escape_debug()
        );
        if self.outcome == GateOutcomeKind::Unknown {
            line.push_str(" — outcome unknown, check `conductor_leaderWithID` before retrying");
        }
        line
    }
}

#[derive(Debug, Serialize)]
struct ProposeRequest<'a> {
    target_id: &'a str,
    justification: &'a str,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    token_url: Option<String>,
}

/// Authenticated client for the approval gateway.
#[derive(Clone)]
pub struct GateClient {
    base: Url,
    token: Arc<Mutex<Option<String>>>,
    /// Where a cached or prompted token is stored; `None` for explicit sources.
    cache: Option<TokenCache>,
    /// The explicit token source the token came from, read again by
    /// [`Self::relogin`]; `None` when the token is cached or prompted.
    explicit: Option<Arc<GateSettings>>,
    /// Token cache writes running in the background, shared by clones.
    cache_writes: Arc<CacheWrites>,
    prompter: Arc<dyn TokenPrompter>,
    http: reqwest::Client,
    retry_backoff: Duration,
    login_timeout: Duration,
    /// Whether a browser login prints its URL to stderr (off in the TUI).
    announce_login: bool,
}

/// Token cache writes for one client and its clones.
#[derive(Default)]
struct CacheWrites {
    /// Completion of the newest write, until [`GateClient::flush_cache`] takes it.
    pending: Mutex<Option<oneshot::Receiver<Result<(), GateError>>>>,
    /// Runs before a write takes the lock, so tests can hold it there.
    #[cfg(test)]
    before_store: Option<StoreHook>,
}

/// Test hook run with the token before a cache write.
#[cfg(test)]
type StoreHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Held while a cache write runs, so no two writes in this process share
/// the cache's temp file.
static CACHE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Generation of every cache write started in this process, from any client.
static CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Newest write generation started for each cache file. A write that a newer
/// one for the same file superseded is skipped, whichever client started it.
static NEWEST_CACHE_WRITE: Mutex<BTreeMap<PathBuf, u64>> = Mutex::new(BTreeMap::new());

impl fmt::Debug for GateClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GateClient").field("base", &self.base).finish_non_exhaustive()
    }
}

impl GateClient {
    /// Extra attempts for propose and status after a 503 (leadership changing).
    pub const MAX_RETRIES: usize = 3;
    /// Delay between those attempts.
    pub const RETRY_BACKOFF: Duration = Duration::from_secs(1);
    /// After the browser opener fails, how long the login listener may still
    /// finish a callback already in progress (an opener can exit non-zero
    /// after the browser opened). A loopback callback that is being read or
    /// answered completes in well under a millisecond; this leaves room for a
    /// busy scheduler while a visibly failed opener still falls back promptly.
    pub const OPENER_FAILED_DRAIN: Duration = Duration::from_millis(200);
    /// Route segment under `/admin/conductor` for proposals.
    pub const PROPOSALS: &str = "proposals";
    /// Maximum proposal ID length.
    pub const MAX_PROPOSAL_ID_LEN: usize = 128;
    /// Maximum justification length in characters, after sanitizing.
    pub const MAX_REASON_CHARS: usize = 200;
    /// Maximum length in characters of gateway error text kept for display.
    pub const MAX_MESSAGE_CHARS: usize = 512;
    /// Timeout for the read-only cluster view, matching the gateway's own
    /// per-request budget for that route.
    pub const CLUSTER_TIMEOUT: Duration = Duration::from_secs(3);
    /// How long [`Self::flush_cache`] waits for a pending cache write.
    pub const CACHE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

    /// Overrides the delay between 503 retries.
    pub const fn with_retry_backoff(mut self, backoff: Duration) -> Self {
        self.retry_backoff = backoff;
        self
    }

    /// Overrides how long to wait for the browser login callback.
    pub const fn with_login_timeout(mut self, timeout: Duration) -> Self {
        self.login_timeout = timeout;
        self
    }

    /// Strips ANSI escapes and control characters, collapses whitespace, and
    /// caps the length at [`Self::MAX_REASON_CHARS`].
    pub fn sanitize_reason(raw: &str) -> String {
        Self::sanitize_text(raw, Self::MAX_REASON_CHARS)
    }

    /// Gateway-provided error text made safe for a terminal: ANSI escapes and
    /// control characters stripped, whitespace collapsed, and at most
    /// [`Self::MAX_MESSAGE_CHARS`] characters (a proxy's error page can be
    /// arbitrarily long).
    pub fn sanitize_message(raw: &str) -> String {
        Self::sanitize_text(raw, Self::MAX_MESSAGE_CHARS)
    }

    /// Strips ANSI escapes and control characters, collapses whitespace, and
    /// keeps at most `max_chars` characters.
    pub fn sanitize_text(raw: &str, max_chars: usize) -> String {
        let mut cleaned = String::with_capacity(raw.len());
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                if chars.next_if_eq(&'[').is_some() {
                    while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {}
                }
                continue;
            }
            cleaned.push(if c.is_control() { ' ' } else { c });
        }
        cleaned.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max_chars).collect()
    }

    /// Returns an error unless `id` is 1 to [`Self::MAX_PROPOSAL_ID_LEN`]
    /// characters, each accepted by [`Self::is_id_char`].
    pub fn check_id(id: &str) -> Result<(), GateError> {
        let valid = !id.is_empty()
            && id.len() <= Self::MAX_PROPOSAL_ID_LEN
            && id.chars().all(Self::is_id_char);
        if valid { Ok(()) } else { Err(GateError::InvalidProposalId) }
    }

    /// Whether `c` may appear in a proposal ID: `[A-Za-z0-9_-]`.
    pub const fn is_id_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_' || c == '-'
    }

    /// Creates a proposal to transfer leadership to `target_id`.
    pub async fn propose(
        &self,
        target_id: &str,
        reason: &str,
    ) -> Result<ProposeResponse, GateError> {
        let justification = Self::sanitize_reason(reason);
        if justification.is_empty() {
            return Err(GateError::ReasonRequired);
        }
        let body = ProposeRequest { target_id, justification: &justification };
        // A 503 does not prove nothing was stored: after an approval-service
        // timeout the gateway answers 503 and the review may already exist.
        // Retrying can then create a second review; that is harmless, because
        // an unapproved duplicate review cannot be executed. The user can check
        // the review portal or propose again.
        self.send_retrying(Method::POST, &[Self::PROPOSALS], Some(&body)).await
    }

    /// Fetches the status of proposal `id`.
    pub async fn status(&self, id: &str) -> Result<ProposalStatusResponse, GateError> {
        Self::check_id(id)?;
        self.send_retrying::<(), _>(Method::GET, &[Self::PROPOSALS, id], None).await
    }

    /// Fetches the gateway's read-only view of the conductor cluster.
    ///
    /// Never logs in: a 401 is returned as [`GateError::Unauthenticated`] so
    /// the caller decides when to log in (the TUI polls this route).
    pub async fn cluster(&self) -> Result<ClusterResponse, GateError> {
        let token = self.current_token();
        self.send_once::<(), _>(
            Method::GET,
            &["cluster"],
            None,
            token.as_deref(),
            Some(Self::CLUSTER_TIMEOUT),
            false,
        )
        .await
    }

    /// Obtains a new token after a 401 and uses it for later requests. With
    /// an explicit token source, reads that source again and fails if it
    /// returns the rejected token; it never falls back to the browser, a
    /// prompt or the cache. Otherwise logs in (see [`Self::login`]).
    pub async fn relogin(&self, token_url: Option<String>) -> Result<String, GateError> {
        let fresh = match &self.explicit {
            Some(settings) => {
                let fresh = settings.explicit_token().await?.ok_or(GateError::MissingToken)?;
                if self.current_token().as_deref() == Some(fresh.as_str()) {
                    return Err(GateError::Token(
                        "the token source returned the token the gateway rejected".to_string(),
                    ));
                }
                fresh
            }
            None => self.login(token_url).await?,
        };
        *self.token.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(fresh.clone());
        Ok(fresh)
    }

    /// Picks up a usable cached token that differs from the current one, for
    /// example one cached by a CLI login while the TUI was waiting. Returns
    /// whether the token changed. Explicit token sources are not read here;
    /// see [`Self::relogin`].
    pub fn reload_cached_token(&self) -> bool {
        let Some(cached) = self.cache.as_ref().and_then(|cache| cache.load(Utc::now())) else {
            return false;
        };
        let mut token = self.token.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if token.as_deref() == Some(cached.as_str()) {
            return false;
        }
        *token = Some(cached);
        true
    }

    /// Whether the token comes from an explicit source (token file,
    /// `BASECTL_GATE_TOKEN` or `gate_token_command`) rather than the cache.
    pub const fn has_explicit_source(&self) -> bool {
        self.explicit.is_some()
    }

    /// Transfers leadership for an approved proposal. Never retried automatically.
    ///
    /// FAILED and UNKNOWN outcomes (502 with the gateway's JSON) are returned
    /// as `Ok`. Any other server error (5xx other than 503, such as a 500,
    /// 504, a 502 without that JSON, or a proxy's 520-524), a 2xx whose body
    /// isn't a valid execute response, a client timeout, or a connection lost
    /// after sending are reported as UNKNOWN, because execute isn't idempotent
    /// and the transfer may already have happened. A 412 or 503 means nothing
    /// was transferred; other 4xx responses are returned as errors.
    pub async fn execute(&self, id: &str) -> Result<ExecuteResponse, GateError> {
        Self::check_id(id)?;
        let annotate = |status: u16, message: String| GateError::Api { status, message };
        let route = [Self::PROPOSALS, id, "execute"];
        match self.send::<(), _>(Method::POST, &route, None, true).await {
            // Matched before the 5xx arm below: a 503 is refused before any transfer.
            Err(GateError::Api { status: 503, message }) => Err(annotate(
                503,
                format!(
                    "gateway not ready (leadership changing); nothing was transferred, run execute again: {message}"
                ),
            )),
            Err(
                GateError::Api { status: 200..=299 | 500..=599, message }
                | GateError::Interrupted(message),
            ) => Ok(ExecuteResponse {
                id: id.to_string(),
                outcome: GateOutcomeKind::Unknown,
                detail: message,
            }),
            Err(GateError::Api { status: 412, message }) => Err(annotate(
                412,
                format!(
                    "{message} (nothing was transferred; execute can be retried once the target is healthy and the approval is still valid)"
                ),
            )),
            Err(GateError::Api { status: 409, message })
                if message.contains("already executed on this leader") =>
            {
                Err(annotate(
                    409,
                    format!("{message}; check `basectl conductor transfer status {id}`"),
                ))
            }
            other => other,
        }
    }

    /// Sends a request, retrying a 503 up to [`Self::MAX_RETRIES`] times.
    async fn send_retrying<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&B>,
    ) -> Result<T, GateError> {
        let mut attempt = 0;
        loop {
            match self.send(method.clone(), segments, body, false).await {
                Err(GateError::Api { status: 503, .. }) if attempt < Self::MAX_RETRIES => {
                    attempt += 1;
                    tokio::time::sleep(self.retry_backoff).await;
                }
                other => return other,
            }
        }
    }

    /// Sends a request. On a 401 without an explicit token source, logs in
    /// (browser, then prompt), caches the new token, and retries exactly once;
    /// the gateway did nothing on a 401, so the retry is safe for every route.
    /// `outcome_on_502`: see [`Self::send_once`].
    async fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&B>,
        outcome_on_502: bool,
    ) -> Result<T, GateError> {
        let token = self.current_token();
        let first =
            self.send_once(method.clone(), segments, body, token.as_deref(), None, outcome_on_502);
        match first.await {
            Err(GateError::Unauthenticated { token_url, .. }) if self.cache.is_some() => {
                let fresh = self.relogin(token_url).await?;
                self.send_once(method, segments, body, Some(&fresh), None, outcome_on_502).await
            }
            other => other,
        }
    }

    /// Obtains and caches a new token: browser login when the gateway sent a
    /// usable `token_url`, else (or if that fails) the paste prompt on a
    /// terminal, else [`GateError::LoginRequired`].
    async fn login(&self, token_url: Option<String>) -> Result<String, GateError> {
        let cache = self.cache.as_ref().ok_or(GateError::MissingToken)?;
        let mut shown_url = token_url.clone();
        let accepted = token_url
            .as_deref()
            .and_then(|raw| Url::parse(raw).ok())
            .filter(LoginListener::accepts)
            .filter(|_| self.prompter.can_open_browser());
        if let Some(token_url) = accepted {
            let listener = LoginListener::bind(&token_url).await?;
            let login_url = LoginListener::login_url(&token_url, &listener.callback_url()?);
            if self.announce_login {
                eprintln!("Log in to the approval gateway at: {login_url}");
            }
            shown_url = Some(login_url.to_string());
            // The opener waits for `open`/`xdg-open` to exit, which can take
            // as long as the browser stays open, so it runs off the async
            // workers while the callback is served; the login timeout bounds
            // the whole login. Only an opener that failed falls back early.
            let prompter = Arc::clone(&self.prompter);
            let url = login_url.to_string();
            let mut opener = tokio::task::spawn_blocking(move || prompter.open_browser(&url));
            let wait = listener.wait(self.login_timeout);
            tokio::pin!(wait);
            // Biased: a callback that is ready wins over an opener result.
            let received = tokio::select! {
                biased;
                received = &mut wait => received,
                opened = &mut opener => match opened {
                    Ok(Ok(())) => wait.await,
                    // Failed or panicked: finish a callback in progress, then
                    // fall back.
                    _ => tokio::time::timeout(Self::OPENER_FAILED_DRAIN, wait)
                        .await
                        .unwrap_or_else(|_| Err(GateError::Token("browser opener failed".to_string()))),
                },
            };
            if let Ok(received) = received {
                self.cache_in_background(cache, &received.token, received.expires_at);
                return Ok(received.token);
            }
        }
        if !self.prompter.is_interactive() {
            return Err(GateError::LoginRequired { url: shown_url });
        }
        let token = self.prompter.prompt(token_url)?.trim().to_string();
        if token.is_empty() {
            return Err(GateError::MissingToken);
        }
        self.cache_in_background(cache, &token, None);
        Ok(token)
    }

    /// Caches `token` on a detached thread, so slow storage never delays the
    /// request that uses it, and the process never waits for it at exit.
    /// Caching only saves a later login; [`Self::flush_cache`] reports a
    /// failed write.
    fn cache_in_background(
        &self,
        cache: &TokenCache,
        token: &str,
        expires_at: Option<DateTime<Utc>>,
    ) {
        let writes = Arc::clone(&self.cache_writes);
        let (done, pending) = oneshot::channel();
        let generation = {
            let mut slot = writes.pending.lock().unwrap_or_else(PoisonError::into_inner);
            *slot = Some(pending);
            let mut newest = NEWEST_CACHE_WRITE.lock().unwrap_or_else(PoisonError::into_inner);
            let generation = CACHE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
            newest.insert(cache.path.clone(), generation);
            generation
        };
        let (cache, token) = (cache.clone(), token.to_string());
        // A thread that cannot start drops `done`; the flush reports that.
        let _ = thread::Builder::new().name("gate-token-cache".to_string()).spawn(move || {
            #[cfg(test)]
            if let Some(hook) = &writes.before_store {
                hook(&token);
            }
            let _serial = CACHE_WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            let newest = NEWEST_CACHE_WRITE.lock().unwrap_or_else(PoisonError::into_inner);
            let current = newest.get(&cache.path) == Some(&generation);
            drop(newest);
            let result = if current { cache.store(&token, expires_at) } else { Ok(()) };
            let _ = done.send(result);
        });
    }

    /// Waits up to [`Self::CACHE_FLUSH_TIMEOUT`] for the newest token cache
    /// write and warns if it failed or is still running. Returns at once
    /// when no write is pending; never fails.
    ///
    /// Shutdown policy: a write still pending after the bound is abandoned.
    /// The cache is replaced by rename, so the previous cache file stays
    /// intact, and the cost is one more sign-in next run. The one-shot CLI
    /// commands call this before they return; the TUI does not, because it
    /// signs in long before it quits and a write takes milliseconds.
    pub async fn flush_cache(&self) {
        let pending =
            self.cache_writes.pending.lock().unwrap_or_else(PoisonError::into_inner).take();
        let Some(pending) = pending else { return };
        let error = match tokio::time::timeout(Self::CACHE_FLUSH_TIMEOUT, pending).await {
            Ok(Ok(Ok(()))) => return,
            Ok(Ok(Err(error))) => error.to_string(),
            Ok(Err(_)) => "the cache write did not finish".to_string(),
            Err(_) => format!("still writing after {:?}", Self::CACHE_FLUSH_TIMEOUT),
        };
        warn!(error = %error, "could not cache the approval gateway token");
    }

    fn current_token(&self) -> Option<String> {
        self.token.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Sends one request. A 2xx body is the response. A 502 body is the
    /// response only when `outcome_on_502` is set: execute reports FAILED and
    /// UNKNOWN as a JSON 502, but on other routes a 502 is an error.
    async fn send_once<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&B>,
        token: Option<&str>,
        timeout: Option<Duration>,
        outcome_on_502: bool,
    ) -> Result<T, GateError> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| GateError::Transport("gateway URL cannot be a base".to_string()))?
            .pop_if_empty()
            .extend(["admin", "conductor"])
            .extend(segments);
        let mut request = self.http.request(method, url);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if (status.is_success() || (outcome_on_502 && status == StatusCode::BAD_GATEWAY))
            && let Ok(parsed) = serde_json::from_slice(&bytes)
        {
            return Ok(parsed);
        }
        let parsed = serde_json::from_slice::<ErrorResponse>(&bytes).ok();
        if status == StatusCode::UNAUTHORIZED {
            return Err(GateError::Unauthenticated {
                message: parsed.as_ref().map_or_else(
                    || "unauthenticated".to_string(),
                    |e| Self::sanitize_message(&e.error),
                ),
                token_url: parsed.and_then(|error| error.token_url),
            });
        }
        // Gateway text is stored sanitized, so every Display of it is safe.
        let message = Self::sanitize_message(
            &parsed.map_or_else(|| String::from_utf8_lossy(&bytes).into_owned(), |e| e.error),
        );
        Err(GateError::Api { status: status.as_u16(), message })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::{get, post},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use chrono::Utc;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use url::Url;

    use super::{GateClient, GateError, GateOutcomeKind, GateSettings, ProposalStatusResponse};
    use crate::{MockTokenPrompter, MonitoringConfig, TokenCache};

    fn settings(
        url: Url,
        env_token: Option<&str>,
        cache_root: Option<&std::path::Path>,
    ) -> GateSettings {
        GateSettings {
            url,
            token_file: None,
            token_command: None,
            timeout: GateSettings::DEFAULT_TIMEOUT,
            env_token: env_token.map(str::to_string),
            cache_root: cache_root.map(std::path::Path::to_path_buf),
            token_command_timeout: GateSettings::TOKEN_COMMAND_TIMEOUT,
        }
    }

    /// A prompter that reports `interactive`, may not open a browser, and
    /// panics if asked to prompt or open one.
    fn never_prompts(interactive: bool) -> Arc<MockTokenPrompter> {
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_is_interactive().return_const(interactive);
        prompter.expect_can_open_browser().return_const(false);
        prompter.expect_open_browser().never();
        prompter.expect_prompt().never();
        Arc::new(prompter)
    }

    fn jwt(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("e30.{payload}.sig")
    }

    /// What the mock gateway puts in `token_url` on a 401.
    #[derive(Clone, Copy)]
    enum LoginUrl {
        None,
        External,
        /// `http://<gateway>/login`, accepted because it is loopback.
        Loopback,
    }

    /// Mock gateway on the status route: replies 401 to any token other than
    /// `valid`, and records every Authorization header ("" when absent).
    async fn auth_gateway(valid: &'static str, login: LoginUrl) -> (Url, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token_url = match login {
            LoginUrl::None => None,
            LoginUrl::External => Some("https://tokens.example/new".to_string()),
            LoginUrl::Loopback => Some(format!("http://{address}/login?app=basectl")),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let router = Router::new().route(
            STATUS,
            get(move |headers: HeaderMap| {
                let auth = headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap().to_string())
                    .unwrap_or_default();
                log.lock().unwrap().push(auth.clone());
                let token_url = token_url.clone();
                async move {
                    if auth == format!("Bearer {valid}") {
                        (StatusCode::OK, status_json("pending").to_string())
                    } else {
                        let mut body = json!({"error": "unauthenticated"});
                        if let Some(url) = token_url {
                            body["token_url"] = json!(url);
                        }
                        (StatusCode::UNAUTHORIZED, body.to_string())
                    }
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (Url::parse(&format!("http://{address}/")).unwrap(), seen)
    }

    /// Plays the login page: posts `{token, expiresAt}` to the callback URL
    /// carried in `login_url`, including its `state` query.
    fn post_callback(login_url: &str, token: &str) {
        let login_url = Url::parse(login_url).unwrap();
        let callback = login_url
            .query_pairs()
            .find(|(key, _)| key == "callback_url")
            .map(|(_, value)| Url::parse(&value).unwrap())
            .unwrap();
        let body = json!({"token": token, "expiresAt": "2999-01-01T00:00:00Z"}).to_string();
        let request = format!(
            "POST {}?{} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            callback.path(),
            callback.query().unwrap(),
            body.len()
        );
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap())).unwrap();
        std::io::Write::write_all(&mut stream, request.as_bytes()).unwrap();
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
    }

    const ID: &str = "rev_0123-ABC";
    const PROPOSALS: &str = "/admin/conductor/proposals";
    const STATUS: &str = "/admin/conductor/proposals/{id}";
    const EXECUTE: &str = "/admin/conductor/proposals/{id}/execute";

    fn status_json(status: &str) -> Value {
        json!({
            "id": ID, "status": status, "title": "Transfer leader to seq-1", "author": "alice",
            "created_at": "2026-01-01T00:00:00Z",
            "action": {"action": "transfer_leader", "target_id": "seq-1", "justification": "maintenance",
                       "network": "devnet", "from_leader_id": "seq-0"},
            "review_url": "https://review.example/r/1",
            "extra_field": true,
        })
    }

    async fn client_with_timeout(router: Router, timeout: Duration) -> GateClient {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, "secret\n").unwrap();
        let settings = GateSettings {
            url: Url::parse(&format!("http://{address}/")).unwrap(),
            token_file: Some(token_file),
            token_command: None,
            timeout,
            env_token: None,
            cache_root: None,
            token_command_timeout: GateSettings::TOKEN_COMMAND_TIMEOUT,
        };
        settings.client().await.unwrap().with_retry_backoff(Duration::from_millis(1))
    }

    /// Serves `responses` in order on `route` (the last one repeats), checks
    /// the bearer token, and counts requests.
    async fn scripted(
        route: &str,
        responses: Vec<(u16, String)>,
    ) -> (GateClient, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let handler = move |headers: HeaderMap| {
            assert_eq!(headers["authorization"], "Bearer secret");
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (status, body) = responses[n.min(responses.len() - 1)].clone();
            async move { (StatusCode::from_u16(status).unwrap(), body) }
        };
        let router = Router::new().route(route, get(handler.clone()).post(handler));
        (client_with_timeout(router, GateSettings::DEFAULT_TIMEOUT).await, hits)
    }

    async fn execute_with(
        status: u16,
        body: &str,
    ) -> (Result<super::ExecuteResponse, GateError>, usize) {
        let (client, hits) = scripted(EXECUTE, vec![(status, body.to_string())]).await;
        let result = client.execute(ID).await;
        (result, hits.load(Ordering::SeqCst))
    }

    fn error(error: &str) -> String {
        json!({ "error": error }).to_string()
    }

    #[tokio::test]
    async fn propose_sends_exact_body_and_prints_review_hint() {
        let router = Router::new().route(
            PROPOSALS,
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer secret");
                assert_eq!(
                    body,
                    json!({"target_id": "seq-1", "justification": "host maintenance"})
                );
                (
                    StatusCode::CREATED,
                    Json(json!({"id": ID, "review_url": "https://review.example/r/1"})),
                )
            }),
        );
        let client = client_with_timeout(router, GateSettings::DEFAULT_TIMEOUT).await;

        let created = client.propose("seq-1", "host\u{1b}[31m\r\nmaintenance").await.unwrap();

        assert_eq!(created.id, ID);
        assert_eq!(
            created.next_steps(),
            format!(
                "ask a different engineer to approve it at https://review.example/r/1, then run `basectl conductor transfer execute {ID}`"
            )
        );
    }

    #[tokio::test]
    async fn propose_retries_unavailable_then_succeeds() {
        let unavailable = (503, error("approval service unavailable"));
        let created = (201, json!({"id": ID}).to_string());
        let (client, hits) =
            scripted(PROPOSALS, vec![unavailable.clone(), unavailable, created]).await;

        let response = client.propose("seq-1", "maintenance").await.unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 3);
        assert!(response.next_steps().contains("in the approval portal"));
    }

    #[tokio::test]
    async fn status_shows_approval_outcome_and_in_progress() {
        let mut done = status_json("approved");
        done["approval"] = json!({"approvers": ["bob"], "approved_at": "2026-01-01T00:05:00Z"});
        done["outcome"] = json!({"caller": "alice", "outcome": "SUCCEEDED", "detail": "seq-1 leads", "at": "2026-01-01T00:06:00Z"});
        let mut running = done.clone();
        running["outcome"] = json!({"caller": "alice", "at": "2026-01-01T00:06:00Z"});
        let (client, _) =
            scripted(STATUS, vec![(200, done.to_string()), (200, running.to_string())]).await;

        let status = client.status(ID).await.unwrap();
        let lines = status.lines(status.created_at.unwrap()).join("\n");
        assert!(lines.contains("approved  by bob at 2026-01-01T00:05:00+00:00"));
        assert!(lines.contains("outcome   SUCCEEDED seq-1 leads"));
        assert!(lines.contains("transfer  seq-0 -> seq-1 on devnet"));

        let status = client.status(ID).await.unwrap();
        assert_eq!(status.outcome.as_ref().unwrap().label(), "in progress");
        assert_eq!(status.execute_hint().as_deref(), Some("Already executed: in progress"));
    }

    #[tokio::test]
    async fn status_retries_unavailable_then_succeeds() {
        let (client, hits) = scripted(
            STATUS,
            vec![(503, error("not the leader, retry")), (200, status_json("pending").to_string())],
        )
        .await;

        let status = client.status(ID).await.unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(
            status.execute_hint().as_deref(),
            Some("Not approved yet (status pending); approve at https://review.example/r/1")
        );
    }

    #[tokio::test]
    async fn execute_reports_succeeded_and_json_failed() {
        let ok = json!({"id": ID, "outcome": "SUCCEEDED", "detail": "seq-1 leads"}).to_string();
        let (result, _) = execute_with(200, &ok).await;
        assert_eq!(result.unwrap().outcome, GateOutcomeKind::Succeeded);

        let failed =
            json!({"id": ID, "outcome": "FAILED", "detail": "transfer rejected"}).to_string();
        let (result, _) = execute_with(502, &failed).await;
        let result = result.unwrap();
        assert_eq!(result.outcome, GateOutcomeKind::Failed);
        assert_eq!(result.detail, "transfer rejected");
    }

    #[tokio::test]
    async fn execute_surfaces_gateway_reasons_verbatim() {
        for (status, reason) in
            [(403, "self-approval does not count"), (409, "review is pending, not approved")]
        {
            let (result, _) = execute_with(status, &error(reason)).await;
            assert_eq!(result.unwrap_err(), GateError::Api { status, message: reason.to_string() });
        }
    }

    #[tokio::test]
    async fn only_execute_reads_a_502_body_as_its_response() {
        let unavailable = error("conductor rpc: unreachable");
        let (client, _) =
            scripted("/admin/conductor/cluster", vec![(502, unavailable.clone())]).await;
        assert_eq!(
            client.cluster().await.unwrap_err(),
            GateError::Api { status: 502, message: "conductor rpc: unreachable".to_string() }
        );
        let (client, _) = scripted(STATUS, vec![(502, unavailable.clone())]).await;
        assert!(matches!(client.status(ID).await, Err(GateError::Api { status: 502, .. })));
        let (client, _) = scripted(PROPOSALS, vec![(502, unavailable)]).await;
        assert!(matches!(
            client.propose("seq-1", "maintenance").await,
            Err(GateError::Api { status: 502, .. })
        ));

        for outcome in ["FAILED", "UNKNOWN"] {
            let body = json!({"id": ID, "outcome": outcome, "detail": "d"}).to_string();
            let (result, _) = execute_with(502, &body).await;
            assert_eq!(result.unwrap().detail, "d", "{outcome}");
        }
    }

    #[tokio::test]
    async fn execute_does_not_follow_redirects() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let router = Router::new()
            .route(
                EXECUTE,
                post(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/moved")]) }),
            )
            .route(
                "/moved",
                post(move || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }),
            );
        let client = client_with_timeout(router, GateSettings::DEFAULT_TIMEOUT).await;

        assert!(matches!(client.execute(ID).await, Err(GateError::Api { status: 307, .. })));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn execute_uncertain_responses_are_unknown() {
        for (status, body) in
            [(502, "<html>502 Bad Gateway</html>".to_string()), (504, error("gateway timeout"))]
        {
            let (result, _) = execute_with(status, &body).await;
            let result = result.unwrap();
            assert_eq!(result.outcome, GateOutcomeKind::Unknown);
            assert!(result.message().ends_with("check `conductor_leaderWithID` before retrying"));
        }
    }

    #[tokio::test]
    async fn execute_server_errors_other_than_503_are_unknown() {
        for status in [500, 520, 524] {
            let (result, hits) = execute_with(status, &error("internal error")).await;
            let result = result.unwrap();
            assert_eq!(result.outcome, GateOutcomeKind::Unknown, "{status}");
            assert_eq!(result.detail, "internal error", "{status}");
            assert_eq!(hits, 1, "{status}");
        }
        for status in [403, 404] {
            let (result, _) = execute_with(status, &error("nope")).await;
            assert_eq!(
                result.unwrap_err(),
                GateError::Api { status, message: "nope".to_string() },
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn execute_undecodable_success_is_unknown() {
        let unknown_outcome = json!({"id": ID, "outcome": "MAYBE", "detail": "?"}).to_string();
        for body in [String::new(), unknown_outcome] {
            let (result, hits) = execute_with(200, &body).await;
            let result = result.unwrap();
            assert_eq!(result.outcome, GateOutcomeKind::Unknown, "{body}");
            assert!(result.message().ends_with("check `conductor_leaderWithID` before retrying"));
            assert_eq!(hits, 1);
        }
    }

    #[tokio::test]
    async fn gateway_error_text_is_stored_sanitized_and_capped() {
        let hostile = "denied\u{1b}[2J\u{1b}]0;title\u{7}\r\nnow";
        let (result, _) = execute_with(403, &error(hostile)).await;
        assert_eq!(
            result.unwrap_err(),
            GateError::Api { status: 403, message: "denied]0;title now".to_string() }
        );

        let page = format!("<html>\u{1b}[31m{}</html>", "a".repeat(2000));
        let (result, _) = execute_with(418, &page).await;
        let GateError::Api { status: 418, message } = result.unwrap_err() else { panic!() };
        assert_eq!(message.chars().count(), GateClient::MAX_MESSAGE_CHARS);
        assert!(message.starts_with("<html>aaa") && !message.contains('\u{1b}'), "{message}");

        let (client, _) = scripted(STATUS, vec![(401, error(hostile))]).await;
        let shown = client.status(ID).await.unwrap_err().to_string();
        assert_eq!(shown, "approval gateway returned 401: denied]0;title now");
    }

    #[tokio::test]
    async fn debug_output_never_shows_the_token() {
        let url = Url::parse("https://gate.example/").unwrap();
        let settings = settings(url, Some("sekret-token-value"), None);
        let shown = format!("{settings:?}");
        assert!(!shown.contains("sekret-token-value"), "{shown}");
        assert!(shown.contains("has_env_token: true"), "{shown}");

        let client = settings.client().await.unwrap();
        let shown = format!("{client:?}");
        assert!(!shown.contains("sekret-token-value"), "{shown}");
    }

    #[test]
    fn blank_gate_token_env_counts_as_unset() {
        let mut config = MonitoringConfig::mainnet();
        config.gate_url = Some(Url::parse("https://gate.example/").unwrap());
        for (raw, expected) in [("", None), ("  \n", None), ("tok", Some("tok"))] {
            let env = |key: &str| (key == GateSettings::TOKEN_ENV).then(|| raw.to_string());
            let settings =
                GateSettings::resolve_with_env(&config, None, None, env).unwrap().unwrap();
            assert_eq!(settings.env_token.as_deref(), expected, "{raw:?}");
        }
    }

    #[tokio::test]
    async fn blank_gate_token_env_falls_back_to_the_cache() {
        let root = tempfile::tempdir().unwrap();
        let url = Url::parse("https://gate.example/").unwrap();
        TokenCache::for_gate(root.path(), &url).store("cached", None).unwrap();
        let mut config = MonitoringConfig::mainnet();
        config.gate_url = Some(url);
        let env = |key: &str| (key == GateSettings::TOKEN_ENV).then(String::new);
        let mut settings =
            GateSettings::resolve_with_env(&config, None, None, env).unwrap().unwrap();
        settings.cache_root = Some(root.path().to_path_buf());

        let client = settings.client_with_prompter(never_prompts(false)).await.unwrap();

        assert_eq!(client.current_token().as_deref(), Some("cached"));
    }

    #[test]
    fn invalid_gate_url_env_is_an_error_not_no_gateway() {
        let env = |value: &'static str| {
            move |key: &str| (key == GateSettings::URL_ENV).then(|| value.to_string())
        };
        let mut config = MonitoringConfig::mainnet();
        let from_config = Url::parse("https://gate.example/").unwrap();
        for configured in [None, Some(from_config.clone())] {
            config.gate_url = configured;
            let error = GateSettings::resolve_with_env(&config, None, None, env("gate.example"))
                .unwrap_err();
            assert!(matches!(error, GateError::InvalidEnvUrl(_)));
            assert!(
                error
                    .to_string()
                    .starts_with("BASECTL_GATE_URL is not a valid URL: \"gate.example\""),
                "{error}"
            );
        }

        // An empty value is unset, so config applies; a valid value wins over config.
        let resolved = GateSettings::resolve_with_env(&config, None, None, env(" ")).unwrap();
        assert_eq!(resolved.unwrap().url, from_config);
        let resolved =
            GateSettings::resolve_with_env(&config, None, None, env("https://env.example/"))
                .unwrap();
        assert_eq!(resolved.unwrap().url.as_str(), "https://env.example/");
        config.gate_url = None;
        assert!(GateSettings::resolve_with_env(&config, None, None, |_| None).unwrap().is_none());
    }

    #[test]
    fn gate_url_must_be_https_or_loopback_http_from_every_source() {
        let env = |value: &'static str| {
            move |key: &str| (key == GateSettings::URL_ENV).then(|| value.to_string())
        };
        let mut config = MonitoringConfig::mainnet();
        config.gate_url = None;
        let accepted = [
            "https://gate.example/",
            "http://localhost:4001/",
            "http://127.0.0.1:4001/",
            "http://[::1]:4001/",
        ];
        for raw in accepted {
            let url = Url::parse(raw).unwrap();
            let from_env = GateSettings::resolve_with_env(&config, None, None, env(raw)).unwrap();
            assert_eq!(from_env.unwrap().url, url, "env {raw}");
            let from_flag =
                GateSettings::resolve_with_env(&config, Some(url.clone()), None, |_| None).unwrap();
            assert_eq!(from_flag.unwrap().url, url, "flag {raw}");
        }

        let insecure = Url::parse("http://gate.example/").unwrap();
        let from_env =
            GateSettings::resolve_with_env(&config, None, None, env("http://gate.example/"));
        let from_flag =
            GateSettings::resolve_with_env(&config, Some(insecure.clone()), None, |_| None);
        config.gate_url = Some(insecure);
        let from_config = GateSettings::resolve_with_env(&config, None, None, |_| None);
        for result in [from_env, from_flag, from_config] {
            let error = result.unwrap_err();
            assert!(matches!(error, GateError::InsecureUrl(_)));
            assert_eq!(
                error.to_string(),
                "gate_url must be https:// (http:// only for localhost): http://gate.example/"
            );
        }
    }

    #[test]
    fn insecure_url_error_omits_credentials_and_query() {
        let mut config = MonitoringConfig::mainnet();
        config.gate_url = Some(Url::parse("http://user:secret@gate.example/?t=x").unwrap());

        let shown =
            GateSettings::resolve_with_env(&config, None, None, |_| None).unwrap_err().to_string();

        assert_eq!(
            shown,
            "gate_url must be https:// (http:// only for localhost): http://gate.example/"
        );
        for leaked in ["secret", "user", "t=x", "?t"] {
            assert!(!shown.contains(leaked), "{shown}");
        }
    }

    #[tokio::test]
    async fn hung_token_command_times_out() {
        let url = Url::parse("http://127.0.0.1:9/").unwrap();
        let hung = GateSettings {
            token_command: Some("sleep 120".to_string()),
            token_command_timeout: Duration::from_millis(200),
            ..settings(url, None, None)
        };
        let started = std::time::Instant::now();

        let error = hung.client().await.unwrap_err();

        assert_eq!(error, GateError::Token("token command timed out after 200ms".to_string()));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn execute_client_timeout_is_unknown() {
        let router = Router::new().route(
            EXECUTE,
            post(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                StatusCode::OK
            }),
        );
        let client = client_with_timeout(router, Duration::from_millis(100)).await;

        assert_eq!(client.execute(ID).await.unwrap().outcome, GateOutcomeKind::Unknown);
    }

    #[tokio::test]
    async fn execute_unavailable_is_a_no_op_and_not_retried() {
        let (result, hits) = execute_with(503, &error("not the leader, retry")).await;

        assert_eq!(
            result.unwrap_err(),
            GateError::Api {
                status: 503,
                message: "gateway not ready (leadership changing); nothing was transferred, run execute again: not the leader, retry".to_string()
            }
        );
        assert_eq!(hits, 1);
    }

    #[tokio::test]
    async fn invalid_input_is_rejected_before_any_request() {
        let router =
            Router::new().fallback(|| async { panic!("no request expected") as StatusCode });
        let client = client_with_timeout(router, GateSettings::DEFAULT_TIMEOUT).await;

        assert_eq!(client.execute("a/b").await.unwrap_err(), GateError::InvalidProposalId);
        assert_eq!(
            client.status(&"x".repeat(129)).await.unwrap_err(),
            GateError::InvalidProposalId
        );
        assert_eq!(
            client.propose("seq-1", " \u{1b}[31m\r\n ").await.unwrap_err(),
            GateError::ReasonRequired
        );
    }

    #[tokio::test]
    async fn missing_token_fails_before_any_request() {
        // Port 9 has no listener; reaching it would be a transport error instead.
        let url = Url::parse("http://127.0.0.1:9/").unwrap();
        let empty_command = GateSettings {
            token_command: Some("true".to_string()),
            ..settings(url.clone(), None, None)
        };
        assert_eq!(empty_command.client().await.unwrap_err(), GateError::MissingToken);
        let no_source = settings(url, None, None);
        assert_eq!(
            no_source.client_with_prompter(never_prompts(false)).await.unwrap_err(),
            GateError::MissingToken
        );
    }

    #[test]
    fn self_approval_hint() {
        let mut status: ProposalStatusResponse =
            serde_json::from_value(status_json("approved")).unwrap();
        assert_eq!(status.execute_hint(), None);
        status.approval = serde_json::from_value(json!({"approvers": ["Alice"]})).unwrap();
        assert!(status.execute_hint().unwrap().starts_with("Self-approval does not count"));
    }

    #[tokio::test]
    async fn browser_login_caches_token_and_retries_once() {
        let (url, seen) = auth_gateway("fresh", LoginUrl::Loopback).await;
        let root = tempfile::tempdir().unwrap();
        let opened = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&opened);
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(true);
        prompter.expect_is_interactive().return_const(false);
        prompter.expect_prompt().never();
        prompter.expect_open_browser().times(1).returning(move |login_url| {
            record.lock().unwrap().push(login_url.to_string());
            let login_url = login_url.to_string();
            std::thread::spawn(move || post_callback(&login_url, "fresh"));
            Ok(())
        });
        let client = settings(url.clone(), None, Some(root.path()))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap();

        client.status(ID).await.unwrap();
        client.flush_cache().await;

        // Request 1 has no token (to learn the login URL); request 2 is the
        // single retry with the token from the callback.
        assert_eq!(*seen.lock().unwrap(), vec!["", "Bearer fresh"]);
        let login_url = opened.lock().unwrap()[0].clone();
        assert!(login_url.starts_with(&format!(
            "{url}login?app=basectl&callback_url=http%3A%2F%2F127.0.0.1%3A"
        )));
        let cache = TokenCache::for_gate(root.path(), &url);
        let stored = std::fs::read_to_string(&cache.path).unwrap();
        assert!(stored.contains(r#""expiresAt":"2999-01-01T00:00:00Z""#));
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("fresh"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&cache.path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    /// A cache root whose parent is a file, so every cache write fails
    /// (whoever runs the test, root included).
    fn unwritable_cache_root(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        file.join("cache")
    }

    #[tokio::test]
    async fn cache_write_failure_still_uses_the_new_token() {
        // Browser login.
        let (url, seen) = auth_gateway("fresh", LoginUrl::Loopback).await;
        let dir = tempfile::tempdir().unwrap();
        let root = unwritable_cache_root(&dir);
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(true);
        prompter.expect_is_interactive().return_const(false);
        prompter.expect_prompt().never();
        prompter.expect_open_browser().times(1).returning(|login_url| {
            let login_url = login_url.to_string();
            std::thread::spawn(move || post_callback(&login_url, "fresh"));
            Ok(())
        });
        let client = settings(url.clone(), None, Some(&root))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap();
        client.status(ID).await.unwrap();
        client.flush_cache().await;
        assert_eq!(*seen.lock().unwrap(), vec!["", "Bearer fresh"]);
        assert!(TokenCache::for_gate(&root, &url).load(Utc::now()).is_none());

        // Paste prompt.
        let (url, seen) = auth_gateway("pasted", LoginUrl::None).await;
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(true);
        prompter.expect_is_interactive().return_const(true);
        prompter.expect_open_browser().never();
        prompter.expect_prompt().times(1).returning(|_| Ok("pasted\n".to_string()));
        let client = settings(url, None, Some(&root))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap();
        client.status(ID).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec!["", "Bearer pasted"]);
    }

    /// Makes the client's cache writes of `token` wait, before taking the
    /// write lock, for a message on the returned sender (at most `limit`).
    fn hold_cache_writes(
        client: &mut GateClient,
        token: &'static str,
        limit: Duration,
    ) -> std::sync::mpsc::Sender<()> {
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let hook = move |written: &str| {
            if written == token {
                let _ = held.lock().unwrap().recv_timeout(limit);
            }
        };
        client.cache_writes = Arc::new(super::CacheWrites {
            before_store: Some(Arc::new(hook)),
            ..Default::default()
        });
        release
    }

    #[tokio::test]
    async fn slow_cache_write_never_delays_the_request_and_flush_is_bounded() {
        let (url, seen) = auth_gateway("pasted", LoginUrl::None).await;
        let root = tempfile::tempdir().unwrap();
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(false);
        prompter.expect_is_interactive().return_const(true);
        prompter.expect_prompt().times(1).returning(|_| Ok("pasted".to_string()));
        let mut client = settings(url.clone(), None, Some(root.path()))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap();
        let _release = hold_cache_writes(&mut client, "pasted", Duration::from_secs(4));

        let started = std::time::Instant::now();
        client.status(ID).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        assert_eq!(*seen.lock().unwrap(), vec!["", "Bearer pasted"]);

        let started = std::time::Instant::now();
        client.flush_cache().await;
        let waited = started.elapsed();
        assert!(waited >= GateClient::CACHE_FLUSH_TIMEOUT, "{waited:?}");
        assert!(waited < GateClient::CACHE_FLUSH_TIMEOUT + Duration::from_secs(1), "{waited:?}");
        // The write is still held, so nothing is cached yet.
        assert!(TokenCache::for_gate(root.path(), &url).load(Utc::now()).is_none());
    }

    #[tokio::test]
    async fn an_older_cache_write_never_overwrites_a_newer_one() {
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        let root = tempfile::tempdir().unwrap();
        let client = || async {
            settings(url.clone(), None, Some(root.path()))
                .client_with_prompter(never_prompts(false))
                .await
                .unwrap()
        };
        let (mut held, other) = (client().await, client().await);
        let release = hold_cache_writes(&mut held, "older", Duration::from_secs(10));
        let cache = TokenCache::for_gate(root.path(), &url);
        let elsewhere =
            TokenCache::for_gate(root.path(), &Url::parse("http://other.example").unwrap());
        let run_held = |cache: &TokenCache| {
            held.cache_in_background(cache, "older", None);
            held.cache_writes.pending.lock().unwrap().take().unwrap()
        };

        // Another client caches a newer token for the same gateway while the
        // older write is held: the older write is skipped when it runs.
        let older = run_held(&cache);
        other.cache_in_background(&cache, "newer", None);
        other.flush_cache().await;
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("newer"));
        release.send(()).unwrap();
        older.await.unwrap().unwrap();
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("newer"));

        // A later write to a different cache file does not supersede it.
        let older = run_held(&elsewhere);
        other.cache_in_background(&cache, "newest", None);
        other.flush_cache().await;
        release.send(()).unwrap();
        older.await.unwrap().unwrap();
        assert_eq!(elsewhere.load(Utc::now()).as_deref(), Some("older"));
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("newest"));
    }

    #[tokio::test]
    async fn login_timeout_falls_back_to_prompt_on_terminal() {
        let (url, seen) = auth_gateway("fresh", LoginUrl::Loopback).await;
        let root = tempfile::tempdir().unwrap();
        let cache = TokenCache::for_gate(root.path(), &url);
        cache.store("stale", None).unwrap();
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(true);
        prompter.expect_is_interactive().return_const(true);
        prompter.expect_open_browser().times(1).returning(|_| Ok(()));
        prompter.expect_prompt().times(1).returning(|_| Ok(" fresh \n".to_string()));
        let client = settings(url, None, Some(root.path()))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap()
            .with_login_timeout(Duration::from_millis(200));

        client.status(ID).await.unwrap();
        client.flush_cache().await;

        assert_eq!(*seen.lock().unwrap(), vec!["Bearer stale", "Bearer fresh"]);
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("fresh"));
    }

    #[tokio::test]
    async fn callback_is_served_while_the_opener_is_still_running() {
        let (url, seen) = auth_gateway("fresh", LoginUrl::Loopback).await;
        let root = tempfile::tempdir().unwrap();
        // The opener posts the callback, then stays in the foreground (as
        // `xdg-open` can while the browser runs) until the test releases it.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(true);
        prompter.expect_is_interactive().return_const(false);
        prompter.expect_prompt().never();
        prompter.expect_open_browser().times(1).returning(move |login_url| {
            let login_url = login_url.to_string();
            std::thread::spawn(move || post_callback(&login_url, "fresh"));
            let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(30));
            Ok(())
        });
        let client = settings(url, None, Some(root.path()))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap()
            .with_login_timeout(Duration::from_secs(2));

        let status = tokio::time::timeout(Duration::from_secs(5), client.status(ID)).await;
        let _ = release.send(());

        status.expect("the login timeout bounds the login").unwrap();
        assert_eq!(*seen.lock().unwrap(), vec!["", "Bearer fresh"]);
    }

    #[tokio::test]
    async fn received_callback_wins_over_a_failing_opener() {
        // The opener delivers a valid callback, reads its 200, then fails, as
        // `xdg-open` can after the browser opened. The token must be used.
        let (url, _) = auth_gateway("fresh", LoginUrl::Loopback).await;
        let root = tempfile::tempdir().unwrap();
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(true);
        prompter.expect_is_interactive().return_const(false);
        prompter.expect_prompt().never();
        prompter.expect_open_browser().returning(|login_url| {
            post_callback(login_url, "fresh");
            Err(GateError::Token("opener exited with 1".to_string()))
        });
        let client = settings(url.clone(), None, Some(root.path()))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap()
            .with_login_timeout(Duration::from_secs(5));

        let token_url = format!("{url}login?app=basectl");
        let mut lost = 0;
        for _ in 0..200 {
            if client.relogin(Some(token_url.clone())).await.ok().as_deref() != Some("fresh") {
                lost += 1;
            }
        }
        assert_eq!(lost, 0, "{lost} of 200 received tokens were dropped");
    }

    #[tokio::test]
    async fn stalled_opener_without_callback_falls_back_at_the_login_timeout() {
        let (url, _) = auth_gateway("fresh", LoginUrl::Loopback).await;
        let root = tempfile::tempdir().unwrap();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(true);
        prompter.expect_is_interactive().return_const(false);
        prompter.expect_prompt().never();
        prompter.expect_open_browser().times(1).returning(move |_| {
            let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(30));
            Ok(())
        });
        let client = settings(url.clone(), None, Some(root.path()))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap()
            .with_login_timeout(Duration::from_millis(300));

        let started = std::time::Instant::now();
        let login = client.relogin(Some(format!("{url}login?app=basectl")));
        let result = tokio::time::timeout(Duration::from_secs(5), login).await;
        let elapsed = started.elapsed();
        let _ = release.send(());

        let error = result.expect("the login timeout bounds the login").unwrap_err();
        match error {
            GateError::LoginRequired { url: Some(shown) } => {
                assert!(shown.starts_with(&format!("{url}login?app=basectl&callback_url=")));
            }
            other => panic!("expected LoginRequired with the URL, got {other:?}"),
        }
        assert!(elapsed >= Duration::from_millis(300) && elapsed < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn failed_opener_falls_back_to_prompt_without_waiting() {
        let (url, seen) = auth_gateway("pasted", LoginUrl::Loopback).await;
        let root = tempfile::tempdir().unwrap();
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(true);
        prompter.expect_is_interactive().return_const(true);
        prompter
            .expect_open_browser()
            .times(1)
            .returning(|_| Err(GateError::Token("no opener".to_string())));
        prompter.expect_prompt().times(1).returning(|_| Ok("pasted".to_string()));
        // A login timeout far beyond the check below: the fallback must not
        // wait it out.
        let client = settings(url, None, Some(root.path()))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap()
            .with_login_timeout(Duration::from_secs(60));

        tokio::time::timeout(Duration::from_secs(5), client.status(ID))
            .await
            .expect("a failed opener falls back at once")
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec!["", "Bearer pasted"]);
    }

    #[tokio::test]
    async fn without_terminal_login_failure_lists_url_and_explicit_options() {
        for login in [LoginUrl::Loopback, LoginUrl::None] {
            let (url, seen) = auth_gateway("fresh", login).await;
            let root = tempfile::tempdir().unwrap();
            let mut prompter = MockTokenPrompter::new();
            prompter.expect_can_open_browser().return_const(true);
            prompter.expect_is_interactive().return_const(false);
            prompter.expect_open_browser().returning(|_| Ok(()));
            prompter.expect_prompt().never();
            let client = settings(url.clone(), None, Some(root.path()))
                .client_with_prompter(Arc::new(prompter))
                .await
                .unwrap()
                .with_login_timeout(Duration::from_millis(100));

            let error = client.status(ID).await.unwrap_err().to_string();

            for option in ["--gate-token-file", "BASECTL_GATE_TOKEN", "gate_token_command"] {
                assert!(error.contains(option), "{error}");
            }
            if matches!(login, LoginUrl::Loopback) {
                assert!(error.contains(&format!("open {url}login?app=basectl&callback_url=")));
            }
            assert_eq!(seen.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn unauthorized_external_login_url_without_browser_is_reported() {
        let (url, seen) = auth_gateway("fresh", LoginUrl::External).await;
        let root = tempfile::tempdir().unwrap();
        TokenCache::for_gate(root.path(), &url).store("stale", None).unwrap();
        let client = settings(url, None, Some(root.path()))
            .client_with_prompter(never_prompts(false))
            .await
            .unwrap();

        let error = client.status(ID).await.unwrap_err();

        assert!(error.to_string().contains("open https://tokens.example/new"));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn expired_cached_token_triggers_login_and_is_never_sent() {
        let (url, seen) = auth_gateway("fresh", LoginUrl::External).await;
        let root = tempfile::tempdir().unwrap();
        let cache = TokenCache::for_gate(root.path(), &url);
        let expired = jwt(Utc::now().timestamp() - 60);
        cache.store(&expired, None).unwrap();
        let mut prompter = MockTokenPrompter::new();
        prompter.expect_can_open_browser().return_const(false);
        prompter.expect_is_interactive().return_const(true);
        prompter.expect_open_browser().never();
        prompter
            .expect_prompt()
            .withf(|url| url.as_deref() == Some("https://tokens.example/new"))
            .times(1)
            .returning(|_| Ok("fresh".into()));
        let client = settings(url, None, Some(root.path()))
            .client_with_prompter(Arc::new(prompter))
            .await
            .unwrap();

        client.status(ID).await.unwrap();
        client.flush_cache().await;

        assert_eq!(*seen.lock().unwrap(), vec!["", "Bearer fresh"]);
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("fresh"));
    }

    #[tokio::test]
    async fn explicit_token_wins_and_never_logs_in() {
        let (url, seen) = auth_gateway("fresh", LoginUrl::Loopback).await;
        let root = tempfile::tempdir().unwrap();
        let cache = TokenCache::for_gate(root.path(), &url);
        cache.store("cached", None).unwrap();

        let client = settings(url.clone(), Some("fresh"), Some(root.path()))
            .client_with_prompter(never_prompts(true))
            .await
            .unwrap();
        client.status(ID).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec!["Bearer fresh"]);

        let rejected = settings(url, Some("revoked"), Some(root.path()))
            .client_with_prompter(never_prompts(true))
            .await
            .unwrap();
        assert!(matches!(
            rejected.status(ID).await.unwrap_err(),
            GateError::Unauthenticated { .. }
        ));
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("cached"));
    }

    /// Status shape returned by the gateway, with lowercase review statuses
    /// and the zero values it sends for a self-approved review.
    fn gate_fixture(status: &str, self_approved: bool) -> Value {
        let approval = if self_approved {
            json!({"approvers": null, "approved_at": "0001-01-01T00:00:00Z"})
        } else {
            json!({"approvers": ["bob"], "approved_at": "2026-01-01T00:05:00Z"})
        };
        json!({
            "id": ID, "status": status, "title": "Transfer conductor leader to sequencer-1",
            "author": "alice", "created_at": "2026-01-01T00:00:00Z",
            "action": {"action": "transfer_leader", "target_id": "sequencer-1",
                       "justification": "host maintenance", "network": "devnet",
                       "from_leader_id": "sequencer-0"},
            "approval": approval,
            "outcome": null,
            "review_url": format!("https://review.example/reviews/{ID}"),
        })
    }

    #[test]
    fn lowercase_statuses_are_compared_case_insensitively() {
        let approved: ProposalStatusResponse =
            serde_json::from_value(gate_fixture("approved", false)).unwrap();
        assert!(approved.is_approved());
        assert_eq!(approved.not_approved_warning(), None);
        assert_eq!(approved.execute_hint(), None);
        assert!(approved.lines(Utc::now()).iter().any(|line| line == "status    approved"));

        let pending: ProposalStatusResponse =
            serde_json::from_value(gate_fixture("pending", false)).unwrap();
        assert_eq!(
            pending.not_approved_warning().as_deref(),
            Some("warning: status is pending; the gate will likely refuse")
        );
        assert!(pending.execute_hint().unwrap().starts_with("Not approved yet (status pending)"));
    }

    #[tokio::test]
    async fn self_approved_zero_values_parse_and_execute_surfaces_the_refusal() {
        let (status_client, _) =
            scripted(STATUS, vec![(200, gate_fixture("approved", true).to_string())]).await;
        let status = status_client.status(ID).await.unwrap();
        assert_eq!(status.approval, None);
        assert_eq!(status.outcome, None);
        assert_eq!(status.created_at.unwrap().timestamp(), 1_767_225_600);

        let (result, hits) = execute_with(403, &error("self-approval does not count")).await;

        assert_eq!(
            result.unwrap_err(),
            GateError::Api { status: 403, message: "self-approval does not count".to_string() }
        );
        assert_eq!(hits, 1);
    }

    #[test]
    fn null_and_zero_fields_are_tolerated() {
        let status: ProposalStatusResponse = serde_json::from_value(json!({
            "id": ID, "status": null, "title": null, "author": null, "created_at": "not a time",
            "action": {"target_id": null, "justification": null},
            "approval": {"approvers": ["bob"], "approved_at": "garbage"},
            "outcome": {"caller": null, "outcome": null, "detail": null, "at": "0001-01-01T00:00:00Z"},
            "review_url": "",
        }))
        .unwrap();
        assert_eq!(status.created_at, None);
        assert_eq!(status.review_url, None);
        assert_eq!(status.approval.unwrap().approvers, vec!["bob"]);
        assert_eq!(status.outcome.unwrap().label(), "in progress");
    }
}
