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
        env, fs,
        os::unix::fs::PermissionsExt,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_jwt_secret_creates_file_with_owner_only_permissions() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_dir = env::current_dir().expect("should read current directory");
        let test_dir = unique_temp_dir();

        env::set_current_dir(&test_dir).expect("should enter test directory");
        let secret = JwtSecretReader::default_jwt_secret("l2_jwt.hex");
        env::set_current_dir(original_dir).expect("should restore original directory");

        let secret = secret.expect("should create jwt secret");
        let secret_path = test_dir.join("l2_jwt.hex");
        let mode = fs::metadata(&secret_path)
            .expect("should read jwt secret metadata")
            .permissions()
            .mode()
            & 0o777;
        let content = fs::read_to_string(&secret_path).expect("should read jwt secret file");

        assert_eq!(mode, 0o600);
        assert_eq!(content, alloy_primitives::hex::encode(secret.as_bytes()));

        fs::remove_dir_all(test_dir).expect("should remove test directory");
    }

    #[test]
    fn read_from_path_success() {
        let test_dir = unique_temp_dir();
        let file_path = test_dir.join("test_jwt.hex");
        let expected_secret = JwtSecret::random();

        JwtSecretReader::write_to_path(&file_path, expected_secret).expect("should write secret");
        let loaded_secret =
            JwtSecretReader::read_from_path(&file_path).expect("should read secret");

        assert_eq!(loaded_secret, expected_secret);
        fs::remove_dir_all(test_dir).expect("should remove test directory");
    }

    #[test]
    fn read_from_path_nonexistent_fails() {
        let test_dir = unique_temp_dir();
        let file_path = test_dir.join("nonexistent_jwt.hex");

        let result = JwtSecretReader::read_from_path(&file_path);
        assert!(matches!(result, Err(JwtError::IoError(_))));

        fs::remove_dir_all(test_dir).expect("should remove test directory");
    }

    #[test]
    fn read_from_path_invalid_hex_fails() {
        let test_dir = unique_temp_dir();
        let file_path = test_dir.join("invalid_jwt.hex");
        fs::write(&file_path, "not_valid_hex_content!").expect("should write invalid content");

        let result = JwtSecretReader::read_from_path(&file_path);
        assert!(matches!(result, Err(JwtError::ParseError(_))));

        fs::remove_dir_all(test_dir).expect("should remove test directory");
    }

    #[test]
    fn write_to_path_fails_if_file_already_exists() {
        let test_dir = unique_temp_dir();
        let file_path = test_dir.join("existing_jwt.hex");
        let secret1 = JwtSecret::random();
        let secret2 = JwtSecret::random();

        JwtSecretReader::write_to_path(&file_path, secret1).expect("first write should succeed");
        let second_write = JwtSecretReader::write_to_path(&file_path, secret2);

        assert!(matches!(second_write, Err(JwtError::IoError(_))));
        fs::remove_dir_all(test_dir).expect("should remove test directory");
    }

    #[test]
    fn default_jwt_secret_reads_existing_file() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_dir = env::current_dir().expect("should read current directory");
        let test_dir = unique_temp_dir();
        let existing_secret = JwtSecret::random();
        let secret_path = test_dir.join("jwt.hex");

        JwtSecretReader::write_to_path(&secret_path, existing_secret)
            .expect("should write initial secret");

        env::set_current_dir(&test_dir).expect("should enter test directory");
        let loaded_secret = JwtSecretReader::default_jwt_secret("jwt.hex");
        env::set_current_dir(original_dir).expect("should restore original directory");

        assert_eq!(loaded_secret.expect("should read existing secret"), existing_secret);
        fs::remove_dir_all(test_dir).expect("should remove test directory");
    }

    #[test]
    fn default_jwt_secret_fails_on_corrupt_existing_file() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_dir = env::current_dir().expect("should read current directory");
        let test_dir = unique_temp_dir();
        let secret_path = test_dir.join("jwt.hex");

        fs::write(&secret_path, "corrupt_hex_data").expect("should write corrupt data");

        env::set_current_dir(&test_dir).expect("should enter test directory");
        let result = JwtSecretReader::default_jwt_secret("jwt.hex");
        env::set_current_dir(original_dir).expect("should restore original directory");

        assert!(matches!(result, Err(JwtError::ParseError(_))));
        fs::remove_dir_all(test_dir).expect("should remove test directory");
    }

    #[test]
    fn resolve_jwt_secret_priority() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_dir = env::current_dir().expect("should read current directory");
        let test_dir = unique_temp_dir();

        let file_secret = JwtSecret::random();
        let encoded_secret = JwtSecret::random();
        let default_secret = JwtSecret::random();

        let file_path = test_dir.join("specified_jwt.hex");
        let default_path = test_dir.join("default_jwt.hex");

        JwtSecretReader::write_to_path(&file_path, file_secret).expect("should write file secret");
        JwtSecretReader::write_to_path(&default_path, default_secret)
            .expect("should write default secret");

        env::set_current_dir(&test_dir).expect("should enter test directory");

        // 1. file_path takes priority over encoded and default
        let resolved = JwtSecretReader::resolve_jwt_secret(
            Some(&file_path),
            Some(encoded_secret),
            "default_jwt.hex",
        )
        .expect("should resolve file path");
        assert_eq!(resolved, file_secret);

        // 2. encoded takes priority over default when file_path is None
        let resolved =
            JwtSecretReader::resolve_jwt_secret(None, Some(encoded_secret), "default_jwt.hex")
                .expect("should resolve encoded");
        assert_eq!(resolved, encoded_secret);

        // 3. falls back to default file when both are None
        let resolved = JwtSecretReader::resolve_jwt_secret(None, None, "default_jwt.hex")
            .expect("should resolve default");
        assert_eq!(resolved, default_secret);

        env::set_current_dir(original_dir).expect("should restore original directory");
        fs::remove_dir_all(test_dir).expect("should remove test directory");
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("base-jwt-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).expect("should create test directory");
        path
    }
}
