//! Service-account bearer tokens for the Calendar API.
//!
//! A signed RS256 assertion is exchanged at Google's token endpoint; the result is cached
//! until shortly before it expires. Failures are reported as short, log-safe kinds that
//! never include the key.

use super::{
    CALENDAR_READONLY_SCOPE, TOKEN_URI,
    google::{JsonHttp, TokenSource},
};
use crate::config::{self, ConfigError};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use std::{
    cell::RefCell,
    fmt,
    path::{Path, PathBuf},
};

/// Refresh a bearer token this long before Google says it expires.
const TOKEN_REFRESH_MARGIN: ChronoDuration = ChronoDuration::seconds(60);
/// Google caps service-account assertions at one hour.
const ASSERTION_LIFETIME: ChronoDuration = ChronoDuration::seconds(3600);

/// Why a service-account key could not be turned into a bearer token.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    NotPrivate(#[from] ConfigError),
    #[error("cannot read service-account key {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("service-account key is not the expected JSON shape: {0}")]
    InvalidKey(PathBuf),
    #[error("service-account private key is not a usable RSA PEM: {0}")]
    InvalidPem(PathBuf),
}

/// The fields of a Google service-account JSON key the token flow needs.
#[derive(Debug, Clone, Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    TOKEN_URI.to_owned()
}

#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    expires_at: DateTime<Utc>,
}

/// The JWT claims Google's service-account flow expects.
#[derive(Debug, serde::Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
}

/// Mints and caches a Calendar-read-only bearer token from a service-account key.
pub struct ServiceAccountTokenSource<H: JsonHttp> {
    http: H,
    client_email: String,
    token_uri: String,
    signing_key: jsonwebtoken::EncodingKey,
    clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    cache: RefCell<Option<CachedToken>>,
}

impl<H: JsonHttp> fmt::Debug for ServiceAccountTokenSource<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccountTokenSource")
            .field("client_email", &self.client_email)
            .field("token_uri", &self.token_uri)
            .finish_non_exhaustive()
    }
}

impl<H: JsonHttp> ServiceAccountTokenSource<H> {
    /// Load a private service-account key and prepare to sign with it.
    ///
    /// # Errors
    ///
    /// [`AuthError`] when the key file is missing, not private, not JSON, or its PEM is not
    /// an RSA private key. Messages name the path, never the key.
    pub fn from_key_file(credentials_path: &Path, http: H) -> Result<Self, AuthError> {
        Self::from_key_file_with_clock(credentials_path, http, Utc::now)
    }

    /// As [`Self::from_key_file`], with an injected clock for tests.
    ///
    /// # Errors
    ///
    /// As [`Self::from_key_file`].
    pub fn from_key_file_with_clock(
        credentials_path: &Path,
        http: H,
        clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
    ) -> Result<Self, AuthError> {
        config::require_private_file(credentials_path)?;
        let body = std::fs::read(credentials_path).map_err(|source| AuthError::Unreadable {
            path: credentials_path.to_owned(),
            source,
        })?;
        let key: ServiceAccountKey = serde_json::from_slice(&body)
            .map_err(|_| AuthError::InvalidKey(credentials_path.to_owned()))?;
        let signing_key = jsonwebtoken::EncodingKey::from_rsa_pem(key.private_key.as_bytes())
            .map_err(|_| AuthError::InvalidPem(credentials_path.to_owned()))?;
        Ok(Self {
            http,
            client_email: key.client_email,
            token_uri: key.token_uri,
            signing_key,
            clock: Box::new(clock),
            cache: RefCell::new(None),
        })
    }

    fn fetch(&self, now: DateTime<Utc>) -> Result<CachedToken, String> {
        let claims = Claims {
            iss: &self.client_email,
            scope: CALENDAR_READONLY_SCOPE,
            aud: &self.token_uri,
            iat: now.timestamp(),
            exp: (now + ASSERTION_LIFETIME).timestamp(),
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let assertion = jsonwebtoken::encode(&header, &claims, &self.signing_key)
            .map_err(|_| "sign".to_owned())?;

        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ];
        let (status, body) = self
            .http
            .post_form(&self.token_uri, &form)
            .map_err(|error| error.kind().to_owned())?;
        if status != 200 {
            return Err(format!("http_{status}"));
        }
        let response: TokenResponse =
            serde_json::from_value(body).map_err(|_| "token_response".to_owned())?;
        Ok(CachedToken {
            value: response.access_token,
            expires_at: now + ChronoDuration::seconds(response.expires_in),
        })
    }
}

impl<H: JsonHttp> TokenSource for ServiceAccountTokenSource<H> {
    fn bearer_token(&self) -> Result<String, String> {
        let now = (self.clock)();
        if let Some(cached) = self.cache.borrow().as_ref() {
            if cached.expires_at - TOKEN_REFRESH_MARGIN > now {
                return Ok(cached.value.clone());
            }
        }
        let fresh = self.fetch(now)?;
        let value = fresh.value.clone();
        *self.cache.borrow_mut() = Some(fresh);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::super::test_support::{FakeHttp, HttpCall, now};
    use super::super::{CALENDAR_READONLY_SCOPE, TOKEN_URI};
    use super::*;

    fn key_file(dir: &TempDir, mode: u32) -> std::path::PathBuf {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/google_auth/service-account.json");
        let path = dir.path().join("service-account.json");
        fs::copy(source, &path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn service_account_token_source_signs_a_jwt_and_caches_the_token() {
        let dir = TempDir::new().unwrap();
        let http = FakeHttp::new(
            vec![],
            vec![(
                200,
                json!({"access_token": "access-token-stub", "expires_in": 3600, "token_type": "Bearer"}),
            )],
        );
        let source =
            ServiceAccountTokenSource::from_key_file_with_clock(&key_file(&dir, 0o600), &http, now)
                .unwrap();

        let first = source.bearer_token().unwrap();
        let second = source.bearer_token().unwrap();

        assert_eq!(first, "access-token-stub");
        assert_eq!(second, "access-token-stub");
        let calls = http.calls();
        assert_eq!(calls.len(), 1, "second call served from cache");
        let HttpCall::PostForm { url, form } = &calls[0] else {
            panic!("expected a form POST");
        };
        assert_eq!(url, TOKEN_URI);
        assert_eq!(
            form[0],
            (
                "grant_type".to_owned(),
                "urn:ietf:params:oauth:grant-type:jwt-bearer".to_owned()
            )
        );
        assert_eq!(form[1].0, "assertion");

        // The assertion is a real RS256 JWT with the claims Google expects.
        let jwt = &form[1].1;
        let payload = jwt.split('.').nth(1).expect("three-part JWT");
        let claims: Value = serde_json::from_slice(&base64_url_decode(payload)).unwrap();
        assert_eq!(claims["iss"], "sync@test-project.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], CALENDAR_READONLY_SCOPE);
        assert_eq!(claims["aud"], TOKEN_URI);
        assert_eq!(claims["iat"], now().timestamp());
        assert_eq!(claims["exp"], now().timestamp() + 3600);
        let header: Value =
            serde_json::from_slice(&base64_url_decode(jwt.split('.').next().unwrap())).unwrap();
        assert_eq!(header["alg"], "RS256");
    }

    #[test]
    fn service_account_token_source_refreshes_inside_the_margin() {
        let dir = TempDir::new().unwrap();
        let http = FakeHttp::new(
            vec![],
            vec![
                (200, json!({"access_token": "first", "expires_in": 30})),
                (200, json!({"access_token": "second", "expires_in": 3600})),
            ],
        );
        // 30s lifetime is inside the 60s refresh margin, so the cache is never trusted.
        let source =
            ServiceAccountTokenSource::from_key_file_with_clock(&key_file(&dir, 0o600), &http, now)
                .unwrap();

        assert_eq!(source.bearer_token().unwrap(), "first");
        assert_eq!(source.bearer_token().unwrap(), "second");
    }

    #[test]
    fn service_account_token_source_reports_failures_without_the_key() {
        let dir = TempDir::new().unwrap();
        let http = FakeHttp::new(vec![], vec![(401, json!({"error": "invalid_grant"}))]);
        let source =
            ServiceAccountTokenSource::from_key_file_with_clock(&key_file(&dir, 0o600), &http, now)
                .unwrap();

        assert_eq!(source.bearer_token(), Err("http_401".to_owned()));

        let http = FakeHttp::new(vec![], vec![(200, json!({"nope": true}))]);
        let source =
            ServiceAccountTokenSource::from_key_file_with_clock(&key_file(&dir, 0o600), &http, now)
                .unwrap();
        assert_eq!(source.bearer_token(), Err("token_response".to_owned()));
    }

    #[test]
    fn service_account_token_source_requires_a_private_key_file() {
        let dir = TempDir::new().unwrap();
        let error = ServiceAccountTokenSource::from_key_file(
            &key_file(&dir, 0o644),
            FakeHttp::new(vec![], vec![]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("0600"), "{error}");

        let bad = dir.path().join("bad.json");
        fs::write(
            &bad,
            "{\"client_email\": \"x\", \"private_key\": \"not a pem\"}",
        )
        .unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o600)).unwrap();
        let error = ServiceAccountTokenSource::from_key_file(&bad, FakeHttp::new(vec![], vec![]))
            .unwrap_err();
        assert!(error.to_string().contains("RSA PEM"), "{error}");
    }

    /// Minimal base64url decoder for inspecting the JWT; no padding, URL alphabet.
    #[allow(clippy::cast_possible_truncation)] // every shift leaves at most 8 bits
    fn base64_url_decode(input: &str) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let value = |c: u8| -> u32 {
            let position = ALPHABET
                .iter()
                .position(|&a| a == c)
                .expect("base64url alphabet");
            u32::try_from(position).expect("position < 64")
        };
        let mut out = Vec::new();
        let bytes = input.as_bytes();
        for chunk in bytes.chunks(4) {
            let mut acc: u32 = 0;
            for (i, &c) in chunk.iter().enumerate() {
                acc |= value(c) << (18 - 6 * i);
            }
            let n = chunk.len();
            if n >= 2 {
                out.push((acc >> 16) as u8);
            }
            if n >= 3 {
                out.push((acc >> 8) as u8);
            }
            if n == 4 {
                out.push(acc as u8);
            }
        }
        out
    }
}
