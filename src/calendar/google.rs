//! The Calendar v3 `events.list` call over plain HTTPS, one page at a time.
//!
//! [`JsonHttp`] is the two-shape HTTP surface the gateway and the token flow need,
//! expressed without any HTTP-library types so tests can script it. [`UreqJsonHttp`] is
//! the production implementation.

use super::reader::{CalendarGateway, GatewayError};
use crate::flightwall::TransportError;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::{fmt, time::Duration};

const EVENTS_URL_PREFIX: &str = "https://www.googleapis.com/calendar/v3/calendars/";

/// Two shapes of HTTPS round-trip, expressed without any HTTP-library types.
pub trait JsonHttp {
    /// `GET url?query` with a bearer token; returns status and decoded JSON.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when no status was received.
    fn get_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
        bearer: &str,
    ) -> Result<(u16, Value), TransportError>;

    /// `POST url` with a URL-encoded form body; returns status and decoded JSON.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when no status was received.
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), TransportError>;
}

impl<H: JsonHttp + ?Sized> JsonHttp for &H {
    fn get_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
        bearer: &str,
    ) -> Result<(u16, Value), TransportError> {
        (**self).get_json(url, query, bearer)
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), TransportError> {
        (**self).post_form(url, form)
    }
}

/// Something that can produce a bearer token for the Calendar API.
pub trait TokenSource {
    /// A bearer token valid for at least the next request.
    ///
    /// # Errors
    ///
    /// A short, log-safe reason the token could not be obtained.
    fn bearer_token(&self) -> Result<String, String>;
}

impl<S: TokenSource + ?Sized> TokenSource for &S {
    fn bearer_token(&self) -> Result<String, String> {
        (**self).bearer_token()
    }
}

/// The Calendar v3 `events.list` call, one page at a time.
#[derive(Debug)]
pub struct GoogleCalendarGateway<H: JsonHttp, S: TokenSource> {
    http: H,
    tokens: S,
}

impl<H: JsonHttp, S: TokenSource> GoogleCalendarGateway<H, S> {
    pub const fn new(http: H, tokens: S) -> Self {
        Self { http, tokens }
    }
}

impl<H: JsonHttp, S: TokenSource> CalendarGateway for GoogleCalendarGateway<H, S> {
    /// Read one page of single, time-ordered events including cancellations.
    fn list_events_page(
        &self,
        calendar_id: &str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
        page_token: Option<&str>,
    ) -> Result<Value, GatewayError> {
        let bearer = self.tokens.bearer_token().map_err(GatewayError::Token)?;
        let url = format!("{EVENTS_URL_PREFIX}{}/events", percent_encode(calendar_id));
        let time_min = rfc3339(time_min);
        let time_max = rfc3339(time_max);
        let mut query = vec![
            ("timeMin", time_min.as_str()),
            ("timeMax", time_max.as_str()),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("showDeleted", "true"),
        ];
        if let Some(token) = page_token {
            query.push(("pageToken", token));
        }
        let (status, body) = self.http.get_json(&url, &query, &bearer)?;
        if status == 200 {
            Ok(body)
        } else {
            Err(GatewayError::Status(status))
        }
    }
}

/// Percent-encode a calendar id for the path segment. Google ids are `local@domain`;
/// `@` is the only character in them that needs escaping.
fn percent_encode(calendar_id: &str) -> String {
    calendar_id.replace('@', "%40")
}

fn rfc3339(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The production [`JsonHttp`]: HTTPS, normal certificate validation, no redirects.
pub struct UreqJsonHttp {
    agent: ureq::Agent,
}

impl UreqJsonHttp {
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.new_agent(),
        }
    }
}

impl fmt::Debug for UreqJsonHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UreqJsonHttp").finish_non_exhaustive()
    }
}

impl JsonHttp for UreqJsonHttp {
    fn get_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
        bearer: &str,
    ) -> Result<(u16, Value), TransportError> {
        let request = self
            .agent
            .get(url)
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {bearer}"))
            .query_pairs(query.iter().copied());
        decode(request.call())
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), TransportError> {
        let request = self.agent.post(url).header("accept", "application/json");
        decode(request.send_form(form.iter().copied()))
    }
}

fn decode(
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<(u16, Value), TransportError> {
    let response = result.map_err(|error| TransportError::new(ureq_kind(&error)))?;
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

fn ureq_kind(error: &ureq::Error) -> &'static str {
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{FakeHttp, FixedToken, HttpCall, NoToken, now, owned, utc};
    use super::*;

    #[test]
    fn google_gateway_builds_the_events_list_request() {
        let http = FakeHttp::new(vec![(200, json!({"items": []}))], vec![]);
        let gateway = GoogleCalendarGateway::new(&http, FixedToken("tok"));

        let page = gateway
            .list_events_page(
                "friends@example.invalid",
                now(),
                utc(2026, 9, 28, 12, 0),
                Some("p2"),
            )
            .unwrap();

        assert_eq!(page, json!({"items": []}));
        let [HttpCall::Get { url, query, bearer }] = &http.calls()[..] else {
            panic!("expected one GET");
        };
        assert_eq!(
            url,
            "https://www.googleapis.com/calendar/v3/calendars/friends%40example.invalid/events"
        );
        assert_eq!(bearer, "tok");
        assert_eq!(
            *query,
            owned(&[
                ("timeMin", "2026-09-21T12:00:00Z"),
                ("timeMax", "2026-09-28T12:00:00Z"),
                ("singleEvents", "true"),
                ("orderBy", "startTime"),
                ("showDeleted", "true"),
                ("pageToken", "p2"),
            ])
        );
    }

    #[test]
    fn google_gateway_maps_non_200_and_token_failures() {
        let http = FakeHttp::new(vec![(403, json!({}))], vec![]);
        let gateway = GoogleCalendarGateway::new(&http, FixedToken("tok"));
        assert_eq!(
            gateway.list_events_page("c", now(), now(), None),
            Err(GatewayError::Status(403))
        );

        let http = FakeHttp::new(vec![], vec![]);
        let gateway = GoogleCalendarGateway::new(&http, NoToken);
        assert_eq!(
            gateway.list_events_page("c", now(), now(), None),
            Err(GatewayError::Token("http_401".to_owned()))
        );
        assert!(http.calls().is_empty(), "no request without a token");
    }
}
