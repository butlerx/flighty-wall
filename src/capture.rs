//! Turn an authorized HTTPS capture into sanitized, replay-safe `FlightWall` fixtures.
//!
//! The `FlightWall` backend is undocumented, so the contract has to be observed from the
//! owner's own device before any code talks to it. A raw capture is full of credentials,
//! device identifiers, and home coordinates, so nothing from it is ever committed
//! directly: this module keeps the contract shape and throws the secrets away.
//!
//! Header values are dropped wholesale rather than scrubbed, because an unrecognized
//! authorization scheme is exactly the case a pattern-based scrubber would miss.

use crate::redaction::Rules;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// Bodies larger than this are recorded by size only; a fixture needs shape, not bulk.
pub const MAX_BODY_BYTES: usize = 262_144;

/// Body keys whose values are discarded by name, because their contents are never safe.
pub const SENSITIVE_BODY_KEYS: [&str; 30] = [
    // Credentials, which are frequently too short for any pattern to recognise.
    "access_token",
    "api_key",
    "apikey",
    "authorization",
    "client_secret",
    "id_token",
    "password",
    "refresh_token",
    "secret",
    "signature",
    "token",
    // Identifiers that tie a fixture back to the owner's account or hardware.
    "account_id",
    "accountid",
    "device_id",
    "deviceid",
    "device_token",
    "email",
    "phone",
    "push_token",
    "serial",
    "serial_number",
    "session",
    "session_id",
    "user_id",
    "userid",
    // Area-tracking configuration is the owner's home location.
    "lat",
    "latitude",
    "lng",
    "lon",
    "longitude",
];

/// Raised when a capture file cannot be read as a HAR archive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("capture is not a HAR archive: missing 'log' object")]
    MissingLog,
    #[error("capture is not a HAR archive: missing 'log.entries' array")]
    MissingEntries,
    #[error("capture contains a malformed entry")]
    MalformedEntry,
}

/// One sanitized request/response pair, safe to commit as a fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureEntry {
    pub index: usize,
    pub method: String,
    pub host: String,
    pub path: String,
    pub status: i64,
    pub payload: Value,
}

impl CaptureEntry {
    /// A stable, sortable fixture filename for this entry.
    #[must_use]
    pub fn filename(&self) -> String {
        let slug: String = self
            .path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let slug = if slug.is_empty() {
            "root".to_owned()
        } else {
            slug
        };
        let safe: String = slug
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let safe = safe.trim_matches('-').to_lowercase();
        let safe: String = safe.chars().take(60).collect();
        format!(
            "{:03}-{}-{safe}.json",
            self.index,
            self.method.to_lowercase()
        )
    }
}

/// Sanitize every HAR entry, optionally keeping only an explicit host allowlist.
///
/// # Errors
///
/// [`CaptureError`] when the document is not a HAR archive.
pub fn sanitize_har(
    document: &Value,
    sensitive_terms: &[&str],
    hosts: &[&str],
) -> Result<Vec<CaptureEntry>, CaptureError> {
    let allowed: BTreeSet<String> = hosts.iter().map(|h| h.to_lowercase()).collect();
    let rules = Rules::new()
        .with_terms(sensitive_terms)
        .redacting_keys(SENSITIVE_BODY_KEYS);
    Ok(entries(document)?
        .into_iter()
        .enumerate()
        .filter_map(|(i, raw)| entry(i + 1, raw, &rules))
        .filter(|entry| allowed.is_empty() || allowed.contains(&entry.host.to_lowercase()))
        .collect())
}

/// Every host the capture touched, so an allowlist can be chosen deliberately.
///
/// # Errors
///
/// [`CaptureError`] when the document is not a HAR archive.
pub fn observed_hosts(document: &Value) -> Result<Vec<String>, CaptureError> {
    let hosts: BTreeSet<String> = entries(document)?
        .into_iter()
        .map(|raw| split_url(request_url(raw)).0)
        .filter(|host| !host.is_empty())
        .collect();
    Ok(hosts.into_iter().collect())
}

/// Describe the sanitized capture so the discovery document can cite real requests.
#[must_use]
pub fn manifest(entries: &[CaptureEntry], sensitive_terms: &[&str]) -> Value {
    let rules = Rules::new().with_terms(sensitive_terms);
    let hosts: BTreeSet<&str> = entries
        .iter()
        .map(|e| e.host.as_str())
        .filter(|h| !h.is_empty())
        .collect();
    json!({
        "entry_count": entries.len(),
        "hosts": hosts,
        "entries": entries.iter().map(|entry| json!({
            "file": entry.filename(),
            "method": entry.method,
            "host": entry.host,
            "path": rules.scrub_text(&entry.path),
            "status": entry.status,
        })).collect::<Vec<_>>(),
    })
}

fn entries(document: &Value) -> Result<Vec<&Map<String, Value>>, CaptureError> {
    let log = document
        .get("log")
        .and_then(Value::as_object)
        .ok_or(CaptureError::MissingLog)?;
    let raw = log
        .get("entries")
        .and_then(Value::as_array)
        .ok_or(CaptureError::MissingEntries)?;
    raw.iter()
        .map(|item| item.as_object().ok_or(CaptureError::MalformedEntry))
        .collect()
}

fn request_url(raw_entry: &Map<String, Value>) -> &str {
    raw_entry
        .get("request")
        .and_then(|r| r.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// `(host, path)` from a URL: host lower-cased without userinfo or port, path without
/// query or fragment. A URL with no scheme has no host and is all path.
fn split_url(url: &str) -> (String, String) {
    let without_fragment = url.split('#').next().unwrap_or("");
    let without_query = without_fragment.split('?').next().unwrap_or("");
    let Some((_, rest)) = without_query.split_once("://") else {
        return (String::new(), without_query.to_owned());
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase();
    (host, path.to_owned())
}

fn entry(index: usize, raw: &Map<String, Value>, rules: &Rules) -> Option<CaptureEntry> {
    let request = raw.get("request")?.as_object()?;
    let response = raw.get("response")?.as_object()?;

    let (host, raw_path) = split_url(request_url(raw));
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_uppercase();
    let status = response.get("status").and_then(Value::as_i64).unwrap_or(0);
    // Paths routinely embed an account or device id, so they are scrubbed like a body.
    let path = rules.scrub_text(&raw_path);

    let payload = json!({
        "request": {
            "method": method,
            "host": host,
            "path": path,
            "query_keys": query_keys(request),
            "header_names": header_names(request),
            "body": body(request, rules),
        },
        "response": {
            "status": status,
            "header_names": header_names(response),
            "body": body(response, rules),
        },
    });
    Some(CaptureEntry {
        index,
        method,
        host,
        path,
        status,
        payload,
    })
}

/// Header names only. Values may hold tokens under any scheme.
fn header_names(message: &Map<String, Value>) -> Vec<String> {
    names(message, "headers", true)
}

/// Query parameter names only. Values routinely carry ids and tokens.
fn query_keys(request: &Map<String, Value>) -> Vec<String> {
    names(request, "queryString", false)
}

fn names(message: &Map<String, Value>, key: &str, lowercase: bool) -> Vec<String> {
    let set: BTreeSet<String> = message
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .filter(|name| !name.is_empty())
        .map(|name| {
            if lowercase {
                name.to_lowercase()
            } else {
                name.to_owned()
            }
        })
        .collect();
    set.into_iter().collect()
}

fn body(message: &Map<String, Value>, rules: &Rules) -> Value {
    let container = message
        .get("postData")
        .or_else(|| message.get("content"))
        .and_then(Value::as_object);
    let Some(container) = container else {
        return Value::Null;
    };
    let mime = container
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let text = container.get("text").and_then(Value::as_str).unwrap_or("");
    if text.is_empty() {
        return json!({"omitted": "empty", "mime_type": mime});
    }
    let bytes = text.len();
    if bytes > MAX_BODY_BYTES {
        return json!({"omitted": "oversized", "mime_type": mime, "bytes": bytes});
    }
    match serde_json::from_str::<Value>(text) {
        Ok(decoded) => json!({"mime_type": mime, "json": rules.scrub_value(&decoded)}),
        Err(_) => json!({"omitted": "non_json", "mime_type": mime, "bytes": bytes}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn har(entries: &[Value]) -> Value {
        json!({"log": {"version": "1.2", "entries": entries}})
    }

    fn entry_json(
        url: &str,
        method: &str,
        status: i64,
        request_text: Option<&str>,
        response_text: Option<&str>,
    ) -> Value {
        let mut request = json!({
            "method": method,
            "url": url,
            "headers": [
                {"name": "X-Api-Key", "value": "SECRET-KEY-VALUE-123456789"},
                {"name": "user-agent", "value": "TheFlightWall/1"},
            ],
            "queryString": [{"name": "token", "value": "abc"}],
        });
        if let Some(text) = request_text {
            request["postData"] = json!({"mimeType": "application/json", "text": text});
        }
        let mut response = json!({
            "status": status,
            "headers": [{"name": "cf-ray", "value": "ray-id-0001"}],
        });
        if let Some(text) = response_text {
            response["content"] = json!({"mimeType": "application/json", "text": text});
        }
        json!({"request": request, "response": response})
    }

    #[test]
    fn header_values_and_query_values_are_dropped_names_are_kept() {
        let doc = har(&[entry_json(
            "https://api.theflightwall.com/configuration?token=abc",
            "GET",
            200,
            None,
            Some("{}"),
        )]);

        let entries = sanitize_har(&doc, &[], &[]).unwrap();

        let rendered = entries[0].payload.to_string();
        assert!(!rendered.contains("SECRET-KEY"));
        assert!(!rendered.contains("ray-id"));
        assert!(!rendered.contains("abc"));
        assert_eq!(
            entries[0].payload["request"]["header_names"],
            json!(["user-agent", "x-api-key"])
        );
        assert_eq!(
            entries[0].payload["request"]["query_keys"],
            json!(["token"])
        );
        assert_eq!(entries[0].payload["request"]["path"], "/configuration");
    }

    #[test]
    fn sensitive_body_keys_are_redacted_by_name_and_structure_is_kept() {
        let body = r#"{"userId":"fw_ios_abc","request_config":{"radius_request":{"latitude":53.3,"longitude":-6.2,"id":"9d5b8a3e-1f2c-4d5e-8a9b-0c1d2e3f4a5b","radius_km":7.6}}}"#;
        let doc = har(&[entry_json(
            "https://api.theflightwall.com/configuration",
            "POST",
            200,
            Some(body),
            None,
        )]);

        let entries = sanitize_har(&doc, &[], &[]).unwrap();

        let sent = &entries[0].payload["request"]["body"]["json"];
        assert_eq!(sent["userId"], "<redacted>");
        assert_eq!(
            sent["request_config"]["radius_request"]["latitude"],
            "<redacted>"
        );
        // `id` is not in the key list; the UUID pattern catches it instead.
        assert_eq!(
            sent["request_config"]["radius_request"]["id"],
            "<redacted-uuid>"
        );
        assert_eq!(sent["request_config"]["radius_request"]["radius_km"], 7.6);
        assert_eq!(entries[0].method, "POST");
        assert_eq!(entries[0].status, 200);
    }

    #[test]
    fn host_allowlist_filters_and_observed_hosts_lists_everything() {
        let doc = har(&[
            entry_json(
                "https://api.theflightwall.com/configuration",
                "GET",
                200,
                None,
                None,
            ),
            entry_json(
                "https://cdn.theflightwall.com/img.png",
                "GET",
                200,
                None,
                None,
            ),
            entry_json(
                "https://API.TheFlightWall.com:443/plus/sync",
                "GET",
                200,
                None,
                None,
            ),
        ]);

        let kept = sanitize_har(&doc, &[], &["api.theflightwall.com"]).unwrap();
        let hosts = observed_hosts(&doc).unwrap();

        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|e| e.host == "api.theflightwall.com"));
        assert_eq!(
            kept[1].index, 3,
            "index counts every entry, not just kept ones"
        );
        assert_eq!(hosts, ["api.theflightwall.com", "cdn.theflightwall.com"]);
    }

    #[test]
    fn body_shapes_are_summarised_not_copied() {
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let doc = har(&[
            entry_json("https://h/a", "GET", 200, None, Some("")),
            entry_json("https://h/b", "GET", 200, None, Some(&big)),
            entry_json("https://h/c", "GET", 200, None, Some("<html>")),
            json!({"request": {"method": "GET", "url": "https://h/d"}, "response": {"status": 204}}),
        ]);

        let entries = sanitize_har(&doc, &[], &[]).unwrap();

        assert_eq!(entries[0].payload["response"]["body"]["omitted"], "empty");
        assert_eq!(
            entries[1].payload["response"]["body"]["omitted"],
            "oversized"
        );
        assert_eq!(
            entries[2].payload["response"]["body"]["omitted"],
            "non_json"
        );
        assert_eq!(entries[3].payload["response"]["body"], Value::Null);
        assert_eq!(entries[3].payload["request"]["header_names"], json!([]));
    }

    #[test]
    fn sensitive_terms_scrub_paths_and_bodies() {
        let doc = har(&[entry_json(
            "https://h/devices/Living-Room-Wall/config",
            "GET",
            200,
            None,
            Some(r#"{"name":"Living-Room-Wall"}"#),
        )]);

        let entries = sanitize_har(&doc, &["living-room-wall"], &[]).unwrap();

        assert_eq!(entries[0].path, "/devices/<redacted-name>/config");
        assert_eq!(
            entries[0].payload["response"]["body"]["json"]["name"],
            "<redacted-name>"
        );
    }

    #[test]
    fn non_har_documents_are_rejected_by_name() {
        assert_eq!(
            sanitize_har(&json!({}), &[], &[]),
            Err(CaptureError::MissingLog)
        );
        assert_eq!(
            sanitize_har(&json!({"log": {}}), &[], &[]),
            Err(CaptureError::MissingEntries)
        );
        assert_eq!(
            sanitize_har(&json!({"log": {"entries": [1]}}), &[], &[]),
            Err(CaptureError::MalformedEntry)
        );
        // An entry without request/response is skipped, not fatal.
        assert!(
            sanitize_har(&har(&[json!({})]), &[], &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn filenames_are_stable_sortable_and_safe() {
        let entry = |index, method: &str, path: &str| CaptureEntry {
            index,
            method: method.into(),
            host: "h".into(),
            path: path.into(),
            status: 200,
            payload: Value::Null,
        };
        assert_eq!(
            entry(1, "GET", "/configuration").filename(),
            "001-get-configuration.json"
        );
        assert_eq!(
            entry(12, "POST", "/a/b.c/d_e").filename(),
            "012-post-a-b-c-d-e.json"
        );
        assert_eq!(entry(3, "GET", "/").filename(), "003-get-root.json");
        assert_eq!(entry(4, "GET", "").filename(), "004-get-root.json");
        let long = format!("/{}", "x".repeat(100));
        assert_eq!(
            entry(5, "GET", &long).filename().len(),
            "005-get-".len() + 60 + ".json".len()
        );
    }

    #[test]
    fn manifest_lists_hosts_and_files() {
        let doc = har(&[
            entry_json(
                "https://api.theflightwall.com/configuration",
                "GET",
                200,
                None,
                None,
            ),
            entry_json(
                "https://api.theflightwall.com/configuration",
                "POST",
                200,
                None,
                None,
            ),
        ]);
        let entries = sanitize_har(&doc, &[], &[]).unwrap();

        let manifest = manifest(&entries, &[]);

        assert_eq!(manifest["entry_count"], 2);
        assert_eq!(manifest["hosts"], json!(["api.theflightwall.com"]));
        assert_eq!(
            manifest["entries"][1]["file"],
            "002-post-configuration.json"
        );
        assert_eq!(manifest["entries"][1]["method"], "POST");
    }

    #[test]
    fn split_url_separates_host_and_path() {
        assert_eq!(
            split_url("https://User@API.Example.com:443/a/b?x=1#frag"),
            ("api.example.com".into(), "/a/b".into())
        );
        assert_eq!(split_url("https://h"), ("h".into(), String::new()));
        assert_eq!(
            split_url("/relative/path"),
            (String::new(), "/relative/path".into())
        );
        assert_eq!(split_url(""), (String::new(), String::new()));
    }
}
