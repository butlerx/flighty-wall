//! The per-install key pair the app sends, and the private TOML file it is read from.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::config::{self, ConfigError};

#[derive(Clone, PartialEq, Eq)]
pub struct FlightWallCredentials {
    api_key: String,
    user_id: String,
}

impl FlightWallCredentials {
    #[must_use]
    pub fn new(api_key: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            user_id: user_id.into(),
        }
    }

    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    #[must_use]
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
}

impl fmt::Debug for FlightWallCredentials {
    /// Never show the key pair, even in panics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FlightWallCredentials(<redacted>)")
    }
}

/// Why the `FlightWall` credential file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error(transparent)]
    NotPrivate(#[from] ConfigError),
    #[error("flightwall credential file cannot be read {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("flightwall credential file is not valid TOML: {0}")]
    InvalidToml(PathBuf),
    #[error("flightwall credential file is missing {field}: {path}")]
    MissingField { path: PathBuf, field: &'static str },
}

/// Read the per-install key pair from a private TOML file with `api_key` and `user_id`.
///
/// # Errors
///
/// [`CredentialsError`] when the file is missing, not private, not TOML, or lacks either
/// key. Messages name the path and the field, never the values.
pub fn load_credentials(path: &Path) -> Result<FlightWallCredentials, CredentialsError> {
    config::require_private_file(path)?;
    let body = std::fs::read_to_string(path).map_err(|source| CredentialsError::Unreadable {
        path: path.to_owned(),
        source,
    })?;
    let table: toml::Table = body
        .parse()
        .map_err(|_| CredentialsError::InvalidToml(path.to_owned()))?;

    let field = |name: &'static str| {
        table
            .get(name)
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or(CredentialsError::MissingField {
                path: path.to_owned(),
                field: name,
            })
    };
    Ok(FlightWallCredentials::new(
        field("api_key")?,
        field("user_id")?,
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::super::test_support::credentials;
    use super::*;

    #[test]

    fn credentials_debug_is_redacted() {
        assert_eq!(
            format!("{:?}", credentials()),
            "FlightWallCredentials(<redacted>)"
        );
    }

    fn write_credentials(dir: &TempDir, body: &str, mode: u32) -> std::path::PathBuf {
        let path = dir.path().join("flightwall.toml");
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn load_credentials_reads_a_private_toml_file() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(
            &dir,
            "api_key = \" key-value \"\nuser_id = \"fw_ios_x\"\n",
            0o600,
        );

        let loaded = load_credentials(&path).unwrap();

        assert_eq!(loaded.api_key(), "key-value");
        assert_eq!(loaded.user_id(), "fw_ios_x");
    }

    #[test]
    fn load_credentials_rejects_a_world_readable_file() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(&dir, "api_key = \"k\"\nuser_id = \"u\"\n", 0o644);

        let error = load_credentials(&path).unwrap_err();

        assert!(error.to_string().contains("0600"), "{error}");
    }

    #[test]
    fn load_credentials_names_the_missing_field_not_the_values() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(&dir, "api_key = \"very-secret\"\nuser_id = \"  \"\n", 0o600);

        let error = load_credentials(&path).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("user_id"), "{message}");
        assert!(!message.contains("very-secret"), "{message}");
    }

    #[test]
    fn load_credentials_rejects_non_toml() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(&dir, "{\"api_key\": \"k\"}", 0o600);

        let error = load_credentials(&path).unwrap_err();

        assert!(error.to_string().contains("not valid TOML"), "{error}");
    }
}
