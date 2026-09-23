//! Configuration loading and local secret-file validation.
//!
//! Validation is split in two so the pure half can run against untrusted input without
//! probing the host: [`AppConfig::from_toml_str`] only parses and range-checks, while
//! [`load_config`] additionally touches the filesystem to confirm credential files are
//! private and the state directory is writable.

use serde::{
    Deserialize,
    de::{self, Deserializer},
};
use std::{
    fmt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// Raised when service configuration is missing or unsafe.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file does not exist: {0}")]
    MissingFile(PathBuf),
    #[error("cannot read configuration file {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML in {path}: {message}")]
    InvalidToml { path: PathBuf, message: String },
    /// A validation failure rendered as `table.key: reason`, without echoing the value.
    #[error("invalid configuration in {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("credential file does not exist: {0}")]
    MissingCredential(PathBuf),
    #[error("credential path is not a regular file: {0}")]
    CredentialNotRegular(PathBuf),
    #[error("credential file must be mode 0600: {0}")]
    CredentialTooOpen(PathBuf),
    #[error("cannot inspect credential file {path}: {source}")]
    CredentialUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("state directory is not a directory: {0}")]
    StateParentNotDirectory(PathBuf),
    #[error("state directory is not writable: {0}")]
    StateParentNotWritable(PathBuf),
    #[error("cannot inspect state directory {path}: {source}")]
    StateParentUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------------------
// Field-level validators. Each runs inside serde so a failure carries the field path.
// ---------------------------------------------------------------------------------------

/// A filesystem path written as a string in TOML, with a leading `~` expanded.
///
/// TOML has no path type, so the conversion happens here. Whitespace is stripped first so
/// a stray space around a path does not survive as part of the filename.
fn user_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    let raw = String::deserialize(deserializer)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(de::Error::custom("must be a non-empty path"));
    }
    Ok(expand_user(trimmed))
}

/// Expand a leading `~` or `~/` using `$HOME`.
///
/// `~user` forms are left untouched: the daemon runs as one user and only ever needs
/// the caller's own home.
fn expand_user(value: &str) -> PathBuf {
    let Some(rest) = value.strip_prefix('~') else {
        return PathBuf::from(value);
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return PathBuf::from(value);
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let mut path = PathBuf::from(home);
            if let Some(tail) = rest.strip_prefix('/') {
                path.push(tail);
            }
            path
        }
        _ => PathBuf::from(value),
    }
}

/// A string that must contain at least one non-whitespace character, stored trimmed.
fn non_empty_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let raw = String::deserialize(deserializer)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(de::Error::custom("must be a non-empty string"));
    }
    Ok(trimmed.to_owned())
}

/// An integer within `[MIN, MAX]`. TOML integers arrive as `i64`, so a negative value is
/// caught here rather than by an unsigned conversion error that would hide the bounds.
fn bounded<'de, D, const MIN: i64, const MAX: i64>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = i64::deserialize(deserializer)?;
    if (MIN..=MAX).contains(&value) {
        Ok(value)
    } else {
        Err(de::Error::custom(format!(
            "must be between {MIN} and {MAX}"
        )))
    }
}

fn calendar_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = non_empty_string(deserializer)?;
    if value.eq_ignore_ascii_case("primary") {
        return Err(de::Error::custom(
            "must name the dedicated calendar, not primary",
        ));
    }
    Ok(value)
}

fn bare_hostname<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let raw = String::deserialize(deserializer)?;
    if raw.trim().is_empty() {
        return Err(de::Error::custom("must be a non-empty string"));
    }
    let is_bare = !raw.contains('/') && !raw.contains(':') && raw == raw.trim().to_lowercase();
    if !is_bare {
        return Err(de::Error::custom(
            "must be a bare lowercase hostname, no scheme, port, or path",
        ));
    }
    Ok(raw)
}

fn timeout_seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if value > 0.0 && value <= 120.0 {
        Ok(value)
    } else {
        Err(de::Error::custom("must be greater than 0 and at most 120"))
    }
}

// ---------------------------------------------------------------------------------------
// Tables. `deny_unknown_fields` means a typo fails loudly instead of taking a default, and
// serde never coerces between TOML types, so `"120"` is not an integer and `1` is not a bool.
// ---------------------------------------------------------------------------------------

/// Which calendar to read, and which service-account key opens it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Google {
    #[serde(deserialize_with = "calendar_id")]
    pub calendar_id: String,
    #[serde(deserialize_with = "user_path")]
    pub credentials_path: PathBuf,
}

/// Daemon pacing and the dry-run switch that keeps writes off by default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    #[serde(
        default = "Service::default_poll_interval_seconds",
        deserialize_with = "bounded::<_, 30, 86_400>"
    )]
    pub poll_interval_seconds: i64,
    /// How far ahead the calendar is read. This bounds the *read*, not what is
    /// tracked: only flights departing today are ever sent to the wall.
    #[serde(
        default = "Service::default_lookahead_days",
        deserialize_with = "bounded::<_, 1, 90>"
    )]
    pub lookahead_days: i64,
    #[serde(default = "Service::default_dry_run")]
    pub dry_run: bool,
}

impl Service {
    const fn default_poll_interval_seconds() -> i64 {
        120
    }
    const fn default_lookahead_days() -> i64 {
        7
    }
    const fn default_dry_run() -> bool {
        true
    }
}

impl Default for Service {
    fn default() -> Self {
        Self {
            poll_interval_seconds: Self::default_poll_interval_seconds(),
            lookahead_days: Self::default_lookahead_days(),
            dry_run: Self::default_dry_run(),
        }
    }
}

/// Where the daemon keeps its own state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    #[serde(deserialize_with = "user_path")]
    pub state_path: PathBuf,
}

/// Which wall API to talk to and where its per-install key pair lives.
///
/// The contract is recorded in `docs/flightwall-api.md`. The host is pinned to the one
/// the capture observed; anything else is a misconfiguration, not a feature.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlightWall {
    #[serde(deserialize_with = "user_path")]
    pub credentials_path: PathBuf,
    #[serde(
        default = "FlightWall::default_host",
        deserialize_with = "bare_hostname"
    )]
    pub host: String,
    #[serde(
        default = "FlightWall::default_timeout_seconds",
        deserialize_with = "timeout_seconds"
    )]
    pub timeout_seconds: f64,
    #[serde(
        default = "FlightWall::default_user_agent",
        deserialize_with = "non_empty_string"
    )]
    pub user_agent: String,
}

impl FlightWall {
    pub const DEFAULT_HOST: &'static str = "api.theflightwall.com";
    pub const DEFAULT_USER_AGENT: &'static str =
        "TheFlightWall/1 CFNetwork/3860.700.1 Darwin/25.6.0";

    fn default_host() -> String {
        Self::DEFAULT_HOST.to_owned()
    }
    const fn default_timeout_seconds() -> f64 {
        15.0
    }
    fn default_user_agent() -> String {
        Self::DEFAULT_USER_AGENT.to_owned()
    }
}

/// Hard caps that make an oversized calendar response non-authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(
        default = "Limits::default_max_pages",
        deserialize_with = "bounded::<_, 1, 100>"
    )]
    pub max_pages: i64,
    #[serde(
        default = "Limits::default_max_events",
        deserialize_with = "bounded::<_, 1, 10_000>"
    )]
    pub max_events: i64,
    #[serde(
        default = "Limits::default_max_field_chars",
        deserialize_with = "bounded::<_, 256, 1_000_000>"
    )]
    pub max_field_chars: i64,
    #[serde(
        default = "Limits::default_max_snapshot_bytes",
        deserialize_with = "bounded::<_, 1_024, 100_000_000>"
    )]
    pub max_snapshot_bytes: i64,
}

impl Limits {
    const fn default_max_pages() -> i64 {
        10
    }
    const fn default_max_events() -> i64 {
        500
    }
    const fn default_max_field_chars() -> i64 {
        8_192
    }
    const fn default_max_snapshot_bytes() -> i64 {
        1_048_576
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pages: Self::default_max_pages(),
            max_events: Self::default_max_events(),
            max_field_chars: Self::default_max_field_chars(),
            max_snapshot_bytes: Self::default_max_snapshot_bytes(),
        }
    }
}

/// Validated non-secret service settings and credential locations.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub google: Google,
    #[serde(default)]
    pub service: Service,
    pub storage: Storage,
    #[serde(default)]
    pub flightwall: Option<FlightWall>,
    #[serde(default)]
    pub calendar_limits: Limits,
}

impl AppConfig {
    /// Parse and range-check TOML without touching the filesystem.
    ///
    /// # Errors
    ///
    /// [`ParseError::Syntax`] when the body is not TOML; [`ParseError::Invalid`] when it is
    /// the wrong shape or out of range. Messages read `table.key: reason` and never include
    /// the offending value.
    pub fn from_toml_str(body: &str) -> Result<Self, ParseError> {
        let table: toml::Table = body
            .parse()
            .map_err(|error: toml::de::Error| ParseError::Syntax(error.message().to_owned()))?;
        serde_path_to_error::deserialize(table)
            .map_err(|error| ParseError::Invalid(describe(&error)))
    }
}

/// Why a TOML body could not become an [`AppConfig`], before any path is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Not TOML at all.
    Syntax(String),
    /// TOML, but the wrong shape or out of range.
    Invalid(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(message) | Self::Invalid(message) => f.write_str(message),
        }
    }
}

/// Render a validation failure as `table.key: reason`.
///
/// `serde_path_to_error` tracks the path to the field that failed; the inner message is
/// serde's own, which names a missing or unknown key but never quotes a value.
fn describe(error: &serde_path_to_error::Error<toml::de::Error>) -> String {
    let path = error.path().to_string();
    let message = error.inner().message();
    if path.is_empty() || path == "." {
        message.to_owned()
    } else {
        format!("{path}: {message}")
    }
}

// ---------------------------------------------------------------------------------------
// Filesystem checks. These stay outside the model so validation never probes the host.
// ---------------------------------------------------------------------------------------

/// Require a regular file with no group or world permissions.
///
/// # Errors
///
/// When the path is missing, is not a regular file, has any group or world permission bit
/// set, or cannot be inspected.
pub fn require_private_file(path: &Path) -> Result<(), ConfigError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigError::MissingCredential(path.to_owned()));
        }
        Err(source) => {
            return Err(ConfigError::CredentialUnreadable {
                path: path.to_owned(),
                source,
            });
        }
    };
    if !metadata.is_file() {
        return Err(ConfigError::CredentialNotRegular(path.to_owned()));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ConfigError::CredentialTooOpen(path.to_owned()));
    }
    Ok(())
}

/// Load and validate the service's TOML configuration.
///
/// # Errors
///
/// When the file is missing or unreadable, is not valid TOML, fails shape or range
/// validation, names a credential file that is not private, or names a state path whose
/// directory is not writable.
pub fn load_config(path: impl AsRef<Path>) -> Result<AppConfig, ConfigError> {
    let config_path = expand_user(&path.as_ref().to_string_lossy());
    let body = read_config(&config_path)?;

    let config = AppConfig::from_toml_str(&body).map_err(|error| match error {
        ParseError::Syntax(message) => ConfigError::InvalidToml {
            path: config_path.clone(),
            message,
        },
        ParseError::Invalid(message) => ConfigError::Invalid {
            path: config_path.clone(),
            message,
        },
    })?;

    validate_state_parent(&config.storage.state_path)?;
    require_private_file(&config.google.credentials_path)?;
    if let Some(wall) = &config.flightwall {
        require_private_file(&wall.credentials_path)?;
    }
    Ok(config)
}

fn read_config(config_path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(config_path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ConfigError::MissingFile(config_path.to_owned())
        } else {
            ConfigError::Unreadable {
                path: config_path.to_owned(),
                source,
            }
        }
    })
}

/// Require that the state file's directory either is, or can be created under, a
/// writable directory.
fn validate_state_parent(state_path: &Path) -> Result<(), ConfigError> {
    let parent = state_path.parent().unwrap_or_else(|| Path::new("."));
    if parent.exists() && !parent.is_dir() {
        return Err(ConfigError::StateParentNotDirectory(parent.to_owned()));
    }

    // Walk up until something exists; that is what would have to be writable for the
    // daemon to create the missing directories beneath it.
    let mut existing = parent;
    while !existing.exists() {
        match existing.parent() {
            Some(next) if next != existing => existing = next,
            _ => break,
        }
    }

    let is_writable_directory = match std::fs::metadata(existing) {
        Ok(metadata) => {
            metadata.is_dir() && rustix::fs::access(existing, rustix::fs::Access::WRITE_OK).is_ok()
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(ConfigError::StateParentUnreadable {
                path: parent.to_owned(),
                source,
            });
        }
    };
    if !is_writable_directory {
        return Err(ConfigError::StateParentNotWritable(parent.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fmt::Write as _,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };
    use tempfile::TempDir;

    /// A temporary home holding a 0600 credential file and a writable state directory.
    struct Sandbox {
        root: TempDir,
        credentials: PathBuf,
        state_path: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = TempDir::new().expect("tempdir");
            let state_directory = root.path().join("state");
            fs::create_dir(&state_directory).unwrap();
            chmod(&state_directory, 0o700);
            let credentials = root.path().join("service-account.json");
            fs::write(&credentials, "{}").unwrap();
            chmod(&credentials, 0o600);
            Self {
                root,
                credentials,
                state_path: state_directory.join("state.sqlite3"),
            }
        }

        fn root(&self) -> &Path {
            self.root.path()
        }

        fn config_path(&self) -> PathBuf {
            self.root().join("config.toml")
        }

        fn write(&self, body: &str) -> PathBuf {
            let path = self.config_path();
            fs::write(&path, format!("{}\n", body.trim())).unwrap();
            path
        }

        fn write_default(&self) -> PathBuf {
            self.write_with(&Overrides::default())
        }

        fn write_with(&self, overrides: &Overrides) -> PathBuf {
            let credentials = overrides
                .credentials
                .clone()
                .unwrap_or_else(|| self.credentials.display().to_string());
            let state = overrides
                .state
                .clone()
                .unwrap_or_else(|| self.state_path.display().to_string());
            self.write(&template(
                &overrides.calendar_id,
                &credentials,
                &overrides.poll,
                &state,
            ))
        }

        fn read_config(&self) -> String {
            fs::read_to_string(self.config_path()).unwrap()
        }

        /// Write a config carrying a `[flightwall]` table and its own credential file.
        fn with_flightwall(&self, mode: u32, host: Option<&str>) -> PathBuf {
            let wall_credentials = self.root().join("flightwall.toml");
            fs::write(&wall_credentials, "api_key = \"k\"\nuser_id = \"u\"\n").unwrap();
            chmod(&wall_credentials, mode);
            let mut body = template(
                "friends@example.invalid",
                &self.credentials.display().to_string(),
                "120",
                &self.state_path.display().to_string(),
            );
            write!(
                body,
                "\n[flightwall]\ncredentials_path = \"{}\"\n",
                wall_credentials.display()
            )
            .unwrap();
            if let Some(host) = host {
                writeln!(body, "host = \"{host}\"").unwrap();
            }
            self.write(&body)
        }
    }

    struct Overrides {
        poll: String,
        calendar_id: String,
        state: Option<String>,
        credentials: Option<String>,
    }

    impl Default for Overrides {
        fn default() -> Self {
            Self {
                poll: "120".into(),
                calendar_id: "friends@example.invalid".into(),
                state: None,
                credentials: None,
            }
        }
    }

    fn template(calendar_id: &str, credentials: &str, poll: &str, state: &str) -> String {
        format!(
            r#"
[google]
calendar_id = "{calendar_id}"
credentials_path = "{credentials}"

[service]
poll_interval_seconds = {poll}
lookahead_days = 7

[storage]
state_path = "{state}"
"#
        )
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Assert a load fails, and that the message mentions `needle`.
    fn assert_rejects(path: &Path, needle: &str) -> ConfigError {
        let error = load_config(path).expect_err("expected the config to be rejected");
        let message = error.to_string();
        assert!(
            message.contains(needle),
            "expected error mentioning {needle:?}, got: {message}"
        );
        error
    }

    #[test]
    fn load_config_applies_safe_defaults() {
        let sandbox = Sandbox::new();

        let config = load_config(sandbox.write_default()).unwrap();

        assert_eq!(config.google.calendar_id, "friends@example.invalid");
        assert_eq!(config.google.credentials_path, sandbox.credentials);
        assert_eq!(config.service.poll_interval_seconds, 120);
        assert_eq!(config.service.lookahead_days, 7);
        assert!(config.service.dry_run);
        assert_eq!(config.calendar_limits.max_pages, 10);
        assert_eq!(config.calendar_limits.max_events, 500);
        assert_eq!(config.calendar_limits.max_field_chars, 8192);
        assert_eq!(config.calendar_limits.max_snapshot_bytes, 1_048_576);
        assert_eq!(config.storage.state_path, sandbox.state_path);
        assert!(config.flightwall.is_none());
    }

    #[test]
    fn load_config_reads_the_optional_flightwall_table() {
        let sandbox = Sandbox::new();

        let config = load_config(sandbox.with_flightwall(0o600, None)).unwrap();

        let wall = config.flightwall.expect("flightwall table");
        assert_eq!(
            wall.credentials_path,
            sandbox.root().join("flightwall.toml")
        );
        assert_eq!(wall.host, "api.theflightwall.com");
        assert!((wall.timeout_seconds - 15.0).abs() < f64::EPSILON);
        assert!(wall.user_agent.starts_with("TheFlightWall/"));
    }

    #[test]
    fn load_config_rejects_a_flightwall_host_that_is_not_bare() {
        for host in [
            "https://api.theflightwall.com",
            "api.theflightwall.com:443",
            "API.theflightwall.com",
        ] {
            let sandbox = Sandbox::new();
            assert_rejects(
                &sandbox.with_flightwall(0o600, Some(host)),
                "flightwall.host",
            );
        }
    }

    #[test]
    fn load_config_rejects_a_world_readable_flightwall_credential_file() {
        let sandbox = Sandbox::new();
        assert_rejects(&sandbox.with_flightwall(0o644, None), "0600");
    }

    #[test]
    fn load_config_rejects_unsafe_poll_intervals() {
        for poll in ["0", "-1", "29", "86401"] {
            let sandbox = Sandbox::new();
            let path = sandbox.write_with(&Overrides {
                poll: poll.into(),
                ..Default::default()
            });
            assert_rejects(&path, "poll_interval_seconds");
        }
    }

    #[test]
    fn load_config_rejects_primary_calendar() {
        let sandbox = Sandbox::new();
        let path = sandbox.write_with(&Overrides {
            calendar_id: "primary".into(),
            ..Default::default()
        });
        assert_rejects(&path, "dedicated calendar");
    }

    #[test]
    fn load_config_rejects_blank_calendar_id() {
        let sandbox = Sandbox::new();
        let path = sandbox.write_with(&Overrides {
            calendar_id: "   ".into(),
            ..Default::default()
        });
        assert_rejects(&path, "calendar_id");
    }

    #[test]
    fn load_config_rejects_state_parent_that_is_not_a_directory() {
        let sandbox = Sandbox::new();
        let invalid_parent = sandbox.root().join("not-a-directory");
        fs::write(&invalid_parent, "x").unwrap();
        let path = sandbox.write_with(&Overrides {
            state: Some(invalid_parent.join("state.sqlite3").display().to_string()),
            ..Default::default()
        });

        assert_rejects(&path, "state directory");
    }

    #[test]
    fn load_config_rejects_an_unknown_table() {
        let sandbox = Sandbox::new();
        sandbox.write_default();
        let path = sandbox.write(&format!(
            "{}\n[flightwall]\nhost = \"x.invalid\"\n",
            sandbox.read_config()
        ));

        // `[flightwall]` without `credentials_path` is incomplete, so it is rejected by name.
        assert_rejects(&path, "flightwall");
    }

    #[test]
    fn load_config_rejects_an_unknown_key_in_a_known_table() {
        let sandbox = Sandbox::new();
        sandbox.write_default();
        let path = sandbox.write(
            &sandbox
                .read_config()
                .replace("lookahead_days", "lookahead_dys"),
        );

        assert_rejects(&path, "lookahead_dys");
    }

    #[test]
    fn load_config_rejects_a_quoted_integer() {
        let sandbox = Sandbox::new();
        let path = sandbox.write_with(&Overrides {
            poll: "\"120\"".into(),
            ..Default::default()
        });
        assert_rejects(&path, "poll_interval_seconds");
    }

    #[test]
    fn load_config_rejects_an_integer_for_a_boolean() {
        let sandbox = Sandbox::new();
        sandbox.write_default();
        let path = sandbox.write(
            &sandbox
                .read_config()
                .replace("lookahead_days = 7", "lookahead_days = 7\ndry_run = 1"),
        );

        assert_rejects(&path, "dry_run");
    }

    #[test]
    fn load_config_rejects_an_empty_path() {
        let sandbox = Sandbox::new();
        let path = sandbox.write_with(&Overrides {
            state: Some(String::new()),
            ..Default::default()
        });
        assert_rejects(&path, "state_path");
    }

    #[test]
    fn load_config_expands_a_home_relative_path() {
        // `HOME` is process-global, so this test owns it for its duration. Cargo runs tests in
        // threads; the guard serialises the two tests that touch the environment.
        let _guard = env_lock().lock().unwrap();
        let sandbox = Sandbox::new();
        let previous = std::env::var_os("HOME");
        set_home(Some(sandbox.root().as_os_str()));

        let result = load_config(sandbox.write_with(&Overrides {
            credentials: Some("~/service-account.json".into()),
            state: Some("~/state/state.sqlite3".into()),
            ..Default::default()
        }));

        set_home(previous.as_deref());
        let config = result.unwrap();
        assert_eq!(config.google.credentials_path, sandbox.credentials);
        assert_eq!(config.storage.state_path, sandbox.state_path);
    }

    #[test]
    fn require_private_file_rejects_group_or_world_access() {
        let root = TempDir::new().unwrap();
        let secret = root.path().join("secret.json");
        fs::write(&secret, "secret-value").unwrap();
        chmod(&secret, 0o644);

        let error = require_private_file(&secret).unwrap_err();
        assert!(error.to_string().contains("0600"), "{error}");

        chmod(&secret, 0o600);
        require_private_file(&secret).unwrap();
    }

    #[test]
    fn config_debug_contains_paths_not_secret_contents() {
        let sandbox = Sandbox::new();
        fs::write(&sandbox.credentials, "super-secret-value").unwrap();

        let config = load_config(sandbox.write_default()).unwrap();
        let rendered = format!("{config:?}");

        assert!(!rendered.contains("super-secret-value"));
        assert!(rendered.contains(&sandbox.credentials.display().to_string()));
    }

    #[test]
    fn from_toml_str_reports_syntax_errors_separately_from_shape_errors() {
        assert!(matches!(
            AppConfig::from_toml_str("this is = not = toml"),
            Err(ParseError::Syntax(_))
        ));
        assert!(matches!(
            AppConfig::from_toml_str("[google]\n"),
            Err(ParseError::Invalid(_))
        ));
    }

    // ---------------------------------------------------------------------------------------
    // Environment plumbing for the one test that needs `$HOME`.
    // ---------------------------------------------------------------------------------------

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        &LOCK
    }

    /// Rust 2024 marks `set_var` unsafe because it races with concurrent `getenv` callers.
    /// The caller holds `env_lock`, and nothing else in this binary reads `HOME` outside it.
    #[allow(unsafe_code)]
    fn set_home(home: Option<&std::ffi::OsStr>) {
        unsafe {
            match home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
