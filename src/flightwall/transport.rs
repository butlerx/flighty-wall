//! One HTTP round-trip, expressed without any HTTP-library types so tests can script it.
//!
//! [`UreqTransport`] is the production implementation: HTTPS, normal certificate
//! validation, HTTP/1.1, no redirects.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

/// The request did not complete; the outcome of a write is unknown.
///
/// `kind` is a short classifier (`timeout`, `connect`, `redirect_refused`) that ends up in
/// the reason string. It never carries a URL, a header, or a body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("transport failed: {kind}")]
pub struct TransportError {
    kind: String,
}

impl TransportError {
    #[must_use]
    pub fn new(kind: impl Into<String>) -> Self {
        Self { kind: kind.into() }
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }
}

/// The only two verbs the contract has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// Request headers by lower-case name. Every name the client sends is a literal.
pub type Headers = BTreeMap<&'static str, String>;

/// One HTTP round-trip.
pub trait Transport {
    /// Send one request and return the status code and decoded JSON body.
    ///
    /// An empty or undecodable body decodes to `{}`; the status code still tells the caller
    /// what happened.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when the request did not complete: no status was received.
    fn request(
        &self,
        method: Method,
        path: &str,
        headers: &Headers,
        body: Option<&Value>,
    ) -> Result<(u16, Value), TransportError>;
}

impl<T: Transport + ?Sized> Transport for &T {
    fn request(
        &self,
        method: Method,
        path: &str,
        headers: &Headers,
        body: Option<&Value>,
    ) -> Result<(u16, Value), TransportError> {
        (**self).request(method, path, headers, body)
    }
}

/// The production transport: HTTPS, normal certificate validation, HTTP/1.1, no redirects.
pub struct UreqTransport {
    agent: ureq::Agent,
    base_url: String,
}

impl UreqTransport {
    /// A transport pinned to one host.
    #[must_use]
    pub fn new(host: &str, timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.new_agent(),
            base_url: format!("https://{host}"),
        }
    }
}

impl fmt::Debug for UreqTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UreqTransport")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Transport for UreqTransport {
    fn request(
        &self,
        method: Method,
        path: &str,
        headers: &Headers,
        body: Option<&Value>,
    ) -> Result<(u16, Value), TransportError> {
        let url = format!("{}{path}", self.base_url);
        let response = match method {
            Method::Get => {
                let request = headers
                    .iter()
                    .fold(self.agent.get(&url), |request, (name, value)| {
                        request.header(*name, value)
                    });
                request.call()
            }
            Method::Post => {
                let request = headers
                    .iter()
                    .fold(self.agent.post(&url), |request, (name, value)| {
                        request.header(*name, value)
                    });
                let bytes = body
                    .map(serde_json::to_vec)
                    .transpose()
                    .map_err(|_| TransportError::new("encode"))?
                    .unwrap_or_default();
                request.send(&bytes[..])
            }
        }
        .map_err(|error| TransportError::new(transport_kind(&error)))?;

        if response.status().is_redirection() {
            return Err(TransportError::new("redirect_refused"));
        }
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .read_to_vec()
            .map_err(|_| TransportError::new("body"))?;
        let decoded = if bytes.is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::Object(Map::new()))
        };
        Ok((status, decoded))
    }
}

/// A short, log-safe classifier for a transport failure.
fn transport_kind(error: &ureq::Error) -> &'static str {
    match error {
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::ConnectionFailed => "connect",
        ureq::Error::HostNotFound => "dns",
        ureq::Error::Io(_) => "io",
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => "redirect_refused",
        ureq::Error::BodyExceedsLimit(_) => "body_too_large",
        _ => "request",
    }
}
