//! JWT secret loading and generation utilities.

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{fs::OpenOptions, io::Write, path::Path};

use alloy_rpc_types_engine::JwtSecret;

use crate::JwtError;

/// The [`JwtSecretReader`] type encapsulates functionality
/// to read [`JwtSecret`]s from disk.
#[derive(Debug, Clone)]
pub struct JwtSecretReader;

impl JwtSecretReader {
    /// Reads a JWT secret from the specified file path.
    ///
    /// The file should contain a hex-encoded JWT secret.
    pub fn read_from_path(path: impl AsRef<Path>) -> Result<JwtSecret, JwtError> {
        let content = std::fs::read_to_string(path.as_ref())
            .map_err(|e| JwtError::IoError(format!("Failed to read JWT secret file: {e}")))?;
        JwtSecret::from_hex(content).map_err(|e| JwtError::ParseError(e.to_string()))
    }

    /// Writes a JWT secret to a new file.
    ///
    /// On Unix platforms, the file is created with owner-only `0600` permissions.
    pub fn write_to_path(path: impl AsRef<Path>, secret: JwtSecret) -> Result<(), JwtError> {
        let path = path.as_ref();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);

        #[cfg(unix)]
        options.mode(0o600);

        let mut file = options.open(path).map_err(|e| {
            JwtError::IoError(format!("Failed to create JWT secret file {}: {e}", path.display()))
        })?;

        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            JwtError::IoError(format!(
                "Failed to set JWT secret file permissions for {}: {e}",
                path.display()
            ))
        })?;

        file.write_all(alloy_primitives::hex::encode(secret.as_bytes()).as_bytes()).map_err(|e| {
            JwtError::IoError(format!("Failed to write JWT secret to file {}: {e}", path.display()))
        })
    }

    /// Attempts to read a JWT secret from a file in the current directory.
    /// Creates a new random secret if the file doesn't exist.
    ///
    /// # Arguments
    /// * `file_name` - The name of the JWT file (e.g., "jwt.hex", "`l2_jwt.hex`")
    pub fn default_jwt_secret(file_name: &str) -> Result<JwtSecret, JwtError> {
        let cur_dir = std::env::current_dir()
            .map_err(|e| JwtError::IoError(format!("Failed to get current directory: {e}")))?;

        let path = cur_dir.join(file_name);

        std::fs::read_to_string(&path).map_or_else(
            |_| {
                let secret = JwtSecret::random();

                Self::write_to_path(&path, secret)?;

                Ok(secret)
            },
            |content| JwtSecret::from_hex(content).map_err(|e| JwtError::ParseError(e.to_string())),
        )
    }

    /// Resolves a JWT secret from multiple sources with priority:
    /// 1. File path (if Some)
    /// 2. Encoded secret (if Some)
    /// 3. Default file in current directory
    ///
    /// # Arguments
    /// * `file_path` - Optional path to a JWT file
    /// * `encoded` - Optional pre-parsed `JwtSecret`
    /// * `default_file` - Fallback file name in current directory
    pub fn resolve_jwt_secret(
        file_path: Option<&Path>,
        encoded: Option<JwtSecret>,
        default_file: &str,
    ) -> Result<JwtSecret, JwtError> {
        if let Some(path) = file_path {
            return Self::read_from_path(path);
        }

        if let Some(secret) = encoded {
            return Ok(secret);
        }

        Self::default_jwt_secret(default_file)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        env,
        error::Error,
        fs,
        os::unix::fs::PermissionsExt,
        sync::{Mutex, PoisonError},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_jwt_secret_creates_file_with_owner_only_permissions() -> Result<(), Box<dyn Error>> {
        let _guard = CWD_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let original_dir = env::current_dir()?;
        let test_dir = unique_temp_dir()?;

        env::set_current_dir(&test_dir)?;
        let secret = JwtSecretReader::default_jwt_secret("l2_jwt.hex");
        env::set_current_dir(original_dir)?;

        let secret = secret?;
        let secret_path = test_dir.join("l2_jwt.hex");
        let mode = fs::metadata(&secret_path)?.permissions().mode() & 0o777;
        let content = fs::read_to_string(&secret_path)?;

        assert_eq!(mode, 0o600);
        assert_eq!(content, alloy_primitives::hex::encode(secret.as_bytes()));

        fs::remove_dir_all(test_dir)?;
        Ok(())
    }

    fn unique_temp_dir() -> Result<std::path::PathBuf, Box<dyn Error>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = env::temp_dir().join(format!("base-jwt-{}-{nanos}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(path)
    }
}
