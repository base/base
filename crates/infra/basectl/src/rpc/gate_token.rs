//! Bearer-token handling for the approval gateway when no explicit source is
//! set: a per-gateway cache file, the browser opener for the login page, and a
//! no-echo paste prompt as the fallback.

use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::GateError;

/// Obtains a gateway token from the operator: by browser login or by prompt.
#[cfg_attr(test, mockall::automock)]
pub trait TokenPrompter: Send + Sync {
    /// Whether a browser login may be started (false where something else
    /// owns the terminal, like the TUI).
    fn can_open_browser(&self) -> bool;

    /// Opens `url` in the browser; an error means it could not be opened.
    /// May block while the opener runs; the login is served meanwhile.
    fn open_browser(&self, url: &str) -> Result<(), GateError>;

    /// Whether a prompt can be shown (stdin is a terminal and nothing else
    /// owns it).
    fn is_interactive(&self) -> bool;

    /// Prompts for a token, showing `token_url` first when the gateway sent one.
    fn prompt(&self, token_url: Option<String>) -> Result<String, GateError>;
}

/// Prompts on the controlling terminal without echoing the token.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalPrompter;

impl TokenPrompter for TerminalPrompter {
    fn can_open_browser(&self) -> bool {
        true
    }

    /// Runs the platform opener with the URL as a single argument, no shell,
    /// for at most [`Self::OPENER_TIMEOUT`].
    fn open_browser(&self, url: &str) -> Result<(), GateError> {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let mut command = Command::new(opener);
        command.arg(url);
        Self::run_opener(opener, &mut command, Self::OPENER_TIMEOUT)
    }

    fn is_interactive(&self) -> bool {
        io::stdin().is_terminal()
    }

    fn prompt(&self, token_url: Option<String>) -> Result<String, GateError> {
        let mut stderr = io::stderr();
        if let Some(url) = token_url {
            let _ = writeln!(stderr, "Get a token at {}", url.escape_debug());
        }
        let _ = write!(stderr, "Paste token: ");
        let _ = stderr.flush();
        terminal::enable_raw_mode().map_err(|error| GateError::Token(error.to_string()))?;
        let read = Self::read_hidden();
        let _ = terminal::disable_raw_mode();
        let _ = writeln!(stderr);
        read
    }
}

impl TerminalPrompter {
    /// How long the opener may run. `open` and a backgrounding `xdg-open`
    /// exit within a second or two, but `xdg-open` can stay in the foreground
    /// for as long as the browser it started, so this only caps the thread
    /// and child process, not the login.
    pub const OPENER_TIMEOUT: Duration = Duration::from_secs(10);

    /// How often a running opener is checked for exit.
    const OPENER_POLL: Duration = Duration::from_millis(20);

    /// Runs `command` with null stdio. A spawn failure or non-zero exit is an
    /// error. An opener still running after `timeout` is killed and reaped
    /// and counts as opened: the browser it started may well be showing the
    /// login page, and the login timeout still bounds the wait.
    fn run_opener(name: &str, command: &mut Command, timeout: Duration) -> Result<(), GateError> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| GateError::Token(format!("{name}: {error}")))?;
        match Self::wait_bounded(&mut child, timeout)
            .map_err(|error| GateError::Token(format!("{name}: {error}")))?
        {
            Some(status) if status.success() => Ok(()),
            Some(status) => Err(GateError::Token(format!("{name} exited with {status}"))),
            None => {
                tracing::debug!(opener = name, "browser opener still running; stopped it");
                Ok(())
            }
        }
    }

    /// Waits up to `timeout` for `child` to exit. Returns `None` after
    /// killing and reaping a child that was still running.
    fn wait_bounded(
        child: &mut Child,
        timeout: Duration,
    ) -> io::Result<Option<std::process::ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            let now = Instant::now();
            if now >= deadline {
                // Kill fails only if it already exited; `wait` reaps either way.
                let _ = child.kill();
                child.wait()?;
                return Ok(None);
            }
            thread::sleep(Self::OPENER_POLL.min(deadline - now));
        }
    }

    fn read_hidden() -> Result<String, GateError> {
        let mut token = String::new();
        loop {
            let event = event::read().map_err(|error| GateError::Token(error.to_string()))?;
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter => return Ok(token.trim().to_string()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(GateError::Token("token prompt cancelled".to_string()));
                    }
                    KeyCode::Char(c) => token.push(c),
                    KeyCode::Backspace => {
                        token.pop();
                    }
                    _ => {}
                },
                Event::Paste(text) => token.push_str(&text),
                _ => {}
            }
        }
    }
}

/// Never prompts; used where the terminal is owned by something else (the TUI).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPrompt;

impl TokenPrompter for NoPrompt {
    fn can_open_browser(&self) -> bool {
        false
    }

    fn open_browser(&self, _url: &str) -> Result<(), GateError> {
        Err(GateError::MissingToken)
    }

    fn is_interactive(&self) -> bool {
        false
    }

    fn prompt(&self, _token_url: Option<String>) -> Result<String, GateError> {
        Err(GateError::MissingToken)
    }
}

/// Opens the browser for login but never prompts, for the TUI's cluster view,
/// where the TUI owns the terminal.
#[derive(Debug, Clone, Copy, Default)]
pub struct BrowserLogin;

impl TokenPrompter for BrowserLogin {
    fn can_open_browser(&self) -> bool {
        true
    }

    fn open_browser(&self, url: &str) -> Result<(), GateError> {
        TerminalPrompter.open_browser(url)
    }

    fn is_interactive(&self) -> bool {
        false
    }

    fn prompt(&self, _token_url: Option<String>) -> Result<String, GateError> {
        Err(GateError::MissingToken)
    }
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    exp: Option<i64>,
}

/// On-disk cache format. Older caches hold the bare token instead.
#[derive(Serialize, Deserialize)]
struct CacheFile {
    token: String,
    #[serde(rename = "expiresAt", default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
}

/// Cached gateway token at `<cache root>/basectl/gate-token-<host[_port]>`,
/// readable only by the owner, stored as JSON `{"token", "expiresAt"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenCache {
    /// Cache file path.
    pub path: PathBuf,
}

impl TokenCache {
    /// Seconds before expiry at which a cached token is no longer reused.
    pub const EXPIRY_MARGIN_SECS: i64 = 5 * 60;
    /// `expiresAt` numbers above this are epoch milliseconds, else seconds.
    pub const EPOCH_MILLIS_THRESHOLD: f64 = 1e11;

    /// `$XDG_CACHE_HOME`, else `~/.cache`.
    pub fn default_root() -> Option<PathBuf> {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".cache")))
    }

    /// Cache for the gateway at `url`, keyed by its sanitized host and port.
    pub fn for_gate(root: &Path, url: &Url) -> Self {
        let mut key = url.host_str().unwrap_or("gateway").to_string();
        if let Some(port) = url.port() {
            key.push_str(&format!("_{port}"));
        }
        let key: String = key
            .chars()
            .map(
                |c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' },
            )
            .collect();
        Self { path: root.join("basectl").join(format!("gate-token-{key}")) }
    }

    /// Returns the cached token unless it is missing, empty, or within the
    /// expiry margin of its `expiresAt` (or JWT `exp` when absent). Accepts
    /// the older bare-token format.
    ///
    /// A file readable by group or others is tightened to owner-only before use.
    pub fn load(&self, now: DateTime<Utc>) -> Option<String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&self.path).ok()?.permissions().mode();
            if mode & 0o077 != 0 {
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600)).ok()?;
            }
        }
        let contents = fs::read_to_string(&self.path).ok()?;
        let (token, expires_at) = match serde_json::from_str::<CacheFile>(&contents) {
            Ok(file) => (file.token, file.expires_at),
            Err(_) => (contents.trim().to_string(), None),
        };
        let expires_at = expires_at.or_else(|| Self::jwt_expiry(&token));
        let fresh =
            expires_at.is_none_or(|at| now.timestamp() + Self::EXPIRY_MARGIN_SECS < at.timestamp());
        (!token.is_empty() && fresh).then_some(token)
    }

    /// Writes `token` atomically: a 0600 temp file in a 0700 directory, then rename.
    pub fn store(&self, token: &str, expires_at: Option<DateTime<Utc>>) -> Result<(), GateError> {
        let dir = self.path.parent().ok_or_else(|| GateError::Token("bad cache path".into()))?;
        let io_error = |error: io::Error| GateError::Token(format!("{}: {error}", dir.display()));
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
            builder.mode(0o700);
            options.mode(0o600);
            builder.create(dir).map_err(io_error)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
        }
        #[cfg(not(unix))]
        builder.create(dir).map_err(io_error)?;
        let tmp = dir.join(format!(".gate-token-{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut file = options.open(&tmp).map_err(io_error)?;
        let contents = serde_json::to_vec(&CacheFile { token: token.to_string(), expires_at })
            .map_err(|error| GateError::Token(error.to_string()))?;
        file.write_all(&contents).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        fs::rename(&tmp, &self.path).map_err(io_error)
    }

    /// The `exp` of `token` if it is a JWT with a decodable payload.
    /// The signature is not verified; this only decides when to re-login.
    pub fn jwt_expiry(token: &str) -> Option<DateTime<Utc>> {
        token
            .split('.')
            .nth(1)
            .and_then(|payload| URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok())
            .and_then(|bytes| serde_json::from_slice::<JwtClaims>(&bytes).ok())
            .and_then(|claims| claims.exp)
            .and_then(|exp| DateTime::from_timestamp(exp, 0))
    }

    /// Parses an `expiresAt` value: ISO 8601, or epoch seconds or milliseconds
    /// (as a number or numeric string).
    pub fn parse_expires_at(value: &serde_json::Value) -> Option<DateTime<Utc>> {
        let epoch = |n: f64| {
            let millis = if n > Self::EPOCH_MILLIS_THRESHOLD { n } else { n * 1000.0 };
            DateTime::from_timestamp_millis(millis as i64)
        };
        match value {
            serde_json::Value::Number(n) => n.as_f64().and_then(epoch),
            serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s)
                .map(|at| at.with_timezone(&Utc))
                .ok()
                .or_else(|| s.trim().parse::<f64>().ok().and_then(epoch)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use chrono::Utc;
    use url::Url;

    use super::TokenCache;

    #[cfg(unix)]
    #[test]
    fn stalled_opener_is_killed_and_reaped_at_its_bound() {
        use std::{
            process::Command,
            time::{Duration, Instant},
        };

        use super::TerminalPrompter;

        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let started = Instant::now();
        let status =
            TerminalPrompter::wait_bounded(&mut child, Duration::from_millis(200)).unwrap();
        assert_eq!(status, None);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(5));
        // Reaped, not a zombie: the Child holds the status `wait` collected,
        // and it was the kill.
        let reaped = child.try_wait().unwrap().expect("opener reaped");
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&reaped), Some(9));

        // Through the opener path a stalled opener counts as opened; one that
        // exits non-zero or cannot start is an error.
        let mut stalled = Command::new("sleep");
        stalled.arg("30");
        TerminalPrompter::run_opener("sleep", &mut stalled, Duration::from_millis(100)).unwrap();
        let mut failing = Command::new("false");
        TerminalPrompter::run_opener("false", &mut failing, Duration::from_secs(5)).unwrap_err();
        let mut missing = Command::new("basectl-test-no-such-opener");
        TerminalPrompter::run_opener("missing", &mut missing, Duration::from_secs(5)).unwrap_err();
        let mut quick = Command::new("true");
        TerminalPrompter::run_opener("true", &mut quick, Duration::from_secs(5)).unwrap();
    }

    /// Unsigned JWT with the given `exp`.
    fn jwt(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"op","exp":{exp}}}"#));
        format!("e30.{payload}.sig")
    }

    #[test]
    fn cache_path_uses_sanitized_host_and_port() {
        let url = Url::parse("http://127.0.0.1:8080/base/").unwrap();
        let cache = TokenCache::for_gate(std::path::Path::new("/c"), &url);
        assert_eq!(cache.path, std::path::Path::new("/c/basectl/gate-token-127.0.0.1_8080"));
    }

    #[test]
    fn expiry_parses_iso_seconds_and_millis_and_jwt() {
        let parse = |v: serde_json::Value| TokenCache::parse_expires_at(&v).unwrap().timestamp();
        assert_eq!(parse(serde_json::json!("2030-01-01T00:00:00Z")), 1_893_456_000);
        assert_eq!(parse(serde_json::json!("2030-01-01T01:00:00+01:00")), 1_893_456_000);
        assert_eq!(parse(serde_json::json!(1_893_456_000)), 1_893_456_000);
        assert_eq!(parse(serde_json::json!(1_893_456_000_000_i64)), 1_893_456_000);
        assert_eq!(parse(serde_json::json!("1893456000")), 1_893_456_000);
        assert!(TokenCache::parse_expires_at(&serde_json::json!("soon")).is_none());
        assert_eq!(TokenCache::jwt_expiry(&jwt(1_893_456_000)).unwrap().timestamp(), 1_893_456_000);
        assert!(TokenCache::jwt_expiry("opaque").is_none());
    }

    #[test]
    fn load_honors_expiry_margin_and_legacy_format() {
        let root = tempfile::tempdir().unwrap();
        let cache = TokenCache::for_gate(root.path(), &Url::parse("http://gate.example").unwrap());
        let now = Utc::now();
        cache.store("tok", Some(now + chrono::Duration::minutes(4))).unwrap();
        assert_eq!(cache.load(now), None);
        cache.store("tok", Some(now + chrono::Duration::minutes(10))).unwrap();
        assert_eq!(cache.load(now).as_deref(), Some("tok"));
        std::fs::write(&cache.path, jwt(now.timestamp() + 3600)).unwrap();
        assert!(cache.load(now).is_some());
        std::fs::write(&cache.path, jwt(now.timestamp() + 60)).unwrap();
        assert_eq!(cache.load(now), None);
    }

    #[cfg(unix)]
    #[test]
    fn store_creates_owner_only_file_and_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let cache = TokenCache::for_gate(root.path(), &Url::parse("http://gate.example").unwrap());

        cache.store("tok", None).unwrap();

        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode(&cache.path) & 0o777, 0o600);
        assert_eq!(mode(cache.path.parent().unwrap()) & 0o777, 0o700);
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("tok"));

        std::fs::set_permissions(&cache.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(cache.load(Utc::now()).as_deref(), Some("tok"));
        assert_eq!(mode(&cache.path) & 0o777, 0o600);
    }
}
