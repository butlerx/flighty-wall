//! Read the configuration; replace its tracked flights. Nothing else.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use super::contract::{
    CONFIGURATION_PATH, MAX_TRACKED_FLIGHTS, TrackedFlight, WallSnapshot, WriteOutcome,
    classify_status, request_failed, snapshot_from_document,
};
use super::credentials::FlightWallCredentials;
use super::transport::{Headers, Method, Transport};

/// The outcome of one replace, plus the fresh read the caller must reconcile against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteResult {
    pub outcome: WriteOutcome,
    pub snapshot: WallSnapshot,
    pub reason: Option<String>,
}

/// Why the client would not even attempt a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WriteRefused {
    #[error("refusing to write against a non-authoritative snapshot")]
    NonAuthoritative,
    #[error("the wall tracks at most {MAX_TRACKED_FLIGHTS} flights; refusing to send {0}")]
    TooMany(usize),
}

/// Read the configuration; replace its tracked flights. Nothing else.
pub struct FlightWallClient<T: Transport> {
    transport: T,
    credentials: FlightWallCredentials,
    clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    headers: Headers,
}

impl<T: Transport> FlightWallClient<T> {
    /// A client that stamps snapshots with the system clock.
    pub fn new(transport: T, credentials: FlightWallCredentials, user_agent: &str) -> Self {
        Self::with_clock(transport, credentials, user_agent, Utc::now)
    }

    /// A client with an injected clock, for tests and replay.
    pub fn with_clock(
        transport: T,
        credentials: FlightWallCredentials,
        user_agent: &str,
        clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
    ) -> Self {
        let headers = Headers::from([
            ("accept", "application/json".to_owned()),
            ("user-agent", user_agent.to_owned()),
            ("x-api-key", credentials.api_key().to_owned()),
            ("x-user-id", credentials.user_id().to_owned()),
        ]);
        Self {
            transport,
            credentials,
            clock: Box::new(clock),
            headers,
        }
    }

    /// Read the configuration. Authoritative only if it parses and matches the fingerprint.
    pub fn read(&self) -> WallSnapshot {
        let observed_at = (self.clock)();
        match self
            .transport
            .request(Method::Get, CONFIGURATION_PATH, &self.headers, None)
        {
            Err(error) => WallSnapshot::non_authoritative(observed_at, request_failed(&error)),
            Ok((status, body)) => match classify_status(status, &body) {
                Some(failure) => WallSnapshot::non_authoritative(observed_at, failure),
                None => snapshot_from_document(body, observed_at),
            },
        }
    }

    /// POST the snapshot's document with only `tracked_flights` changed, then re-read.
    ///
    /// The document is copied from the snapshot the caller planned against, so the owner's
    /// display and area settings go back exactly as they were read. Writes are last-writer-
    /// wins on the server; keeping the read-to-write window to this one call is the only
    /// mitigation the contract allows.
    ///
    /// # Errors
    ///
    /// [`WriteRefused`] before any request is made: the snapshot is not authoritative, or
    /// more than [`MAX_TRACKED_FLIGHTS`] flights were asked for.
    pub fn replace_tracked_flights(
        &self,
        snapshot: &WallSnapshot,
        flights: &[TrackedFlight],
    ) -> Result<WriteResult, WriteRefused> {
        let document = snapshot
            .document()
            .filter(|_| snapshot.is_authoritative())
            .ok_or(WriteRefused::NonAuthoritative)?;
        if flights.len() > MAX_TRACKED_FLIGHTS {
            return Err(WriteRefused::TooMany(flights.len()));
        }

        let document = self.document_for_write(document, flights);
        let mut headers = self.headers.clone();
        headers.insert("content-type", "application/json".to_owned());

        let result = match self.transport.request(
            Method::Post,
            CONFIGURATION_PATH,
            &headers,
            Some(&document),
        ) {
            // A full body that reached the server applies even if the response never arrived.
            Err(error) => WriteResult {
                outcome: WriteOutcome::Unknown,
                snapshot: self.read(),
                reason: Some(request_failed(&error)),
            },
            Ok((status, body)) => match classify_status(status, &body) {
                Some(failure) => WriteResult {
                    outcome: WriteOutcome::Rejected,
                    snapshot: snapshot.clone(),
                    reason: Some(failure),
                },
                None => WriteResult {
                    outcome: WriteOutcome::Applied,
                    snapshot: self.read(),
                    reason: None,
                },
            },
        };
        Ok(result)
    }

    /// The read document with `meta` dropped, `tracked_flights` replaced, and `userId` added.
    fn document_for_write(&self, document: &Value, flights: &[TrackedFlight]) -> Value {
        let mut document = document.clone();
        let Some(root) = document.as_object_mut() else {
            return document;
        };
        root.remove("meta");

        let payloads = Value::Array(flights.iter().map(TrackedFlight::as_payload).collect());
        let request_config = root
            .entry("request_config")
            .or_insert_with(|| Value::Object(Map::new()));
        if !request_config.is_object() {
            *request_config = Value::Object(Map::new());
        }
        if let Some(request_config) = request_config.as_object_mut() {
            request_config.insert("tracked_flights".to_owned(), payloads);
        }

        root.insert(
            "userId".to_owned(),
            Value::String(self.credentials.user_id().to_owned()),
        );
        document
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use serde_json::{Value, json};

    use super::super::contract::FINGERPRINT_MODEL;
    use super::super::test_support::{credentials, fixture_document, now, response_document};
    use super::super::transport::TransportError;
    use super::*;
    use crate::models::SnapshotAuthority;

    enum Step {
        Reply(u16, Value),
        Fail(&'static str),
    }

    #[derive(Debug, Clone)]
    struct Call {
        method: Method,
        path: String,
        headers: Headers,
        body: Option<Value>,
    }

    struct FakeTransport {
        responses: RefCell<VecDeque<Step>>,
        calls: RefCell<Vec<Call>>,
    }

    impl FakeTransport {
        fn scripted(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                responses: RefCell::new(steps.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        fn methods(&self) -> Vec<Method> {
            self.calls.borrow().iter().map(|call| call.method).collect()
        }
    }

    impl Transport for FakeTransport {
        fn request(
            &self,
            method: Method,
            path: &str,
            headers: &Headers,
            body: Option<&Value>,
        ) -> Result<(u16, Value), TransportError> {
            self.calls.borrow_mut().push(Call {
                method,
                path: path.to_owned(),
                headers: headers.clone(),
                body: body.cloned(),
            });
            match self.responses.borrow_mut().pop_front() {
                None => panic!("unexpected {method:?} {path}"),
                Some(Step::Fail(kind)) => Err(TransportError::new(kind)),
                Some(Step::Reply(status, body)) => Ok((status, body)),
            }
        }
    }

    fn client(transport: &FakeTransport) -> FlightWallClient<&FakeTransport> {
        FlightWallClient::with_clock(transport, credentials(), "TheFlightWall/1 test", now)
    }

    fn ok(body: Value) -> Step {
        Step::Reply(200, body)
    }

    fn invalid_key() -> Step {
        Step::Reply(
            401,
            json!({"success": false, "errors": [{"code": 1102, "message": "Invalid API key"}]}),
        )
    }

    #[test]
    fn read_captured_configuration_is_authoritative() {
        let transport = FakeTransport::scripted([ok(response_document("get-configuration.json"))]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::Authoritative);
        assert_eq!(snapshot.observed_at, now());
        assert_eq!(
            snapshot.fingerprint.model.as_deref(),
            Some(FINGERPRINT_MODEL)
        );
        assert_eq!(snapshot.flight_numbers(), ["EI61"]);
        assert!(snapshot.tracked_flights[0].show_metrics);
        let call = &transport.calls()[0];
        assert_eq!(
            (call.method, call.path.as_str(), &call.body),
            (Method::Get, "/configuration", &None)
        );
        assert_eq!(call.headers["x-api-key"], credentials().api_key());
        assert_eq!(call.headers["x-user-id"], credentials().user_id());
        assert!(call.headers["user-agent"].starts_with("TheFlightWall/"));
    }

    #[test]
    fn read_with_no_tracked_flights_is_authoritative_and_empty() {
        let mut document = response_document("get-configuration.json");
        document["request_config"]["tracked_flights"] = json!([]);
        let transport = FakeTransport::scripted([ok(document)]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::Authoritative);
        assert!(snapshot.tracked_flights.is_empty());
    }

    /// A named mutation of the captured document and the drift fragment it must produce.
    type DriftCase = (&'static str, fn(&mut Value), &'static str);

    #[test]
    fn fingerprint_drift_is_non_authoritative() {
        let cases: [DriftCase; 5] = [
            (
                "model",
                |d| d["display_config"]["model"] = json!("mini-v2"),
                "model",
            ),
            (
                "drop request_config",
                |d| {
                    d.as_object_mut().unwrap().remove("request_config");
                },
                "top-level",
            ),
            (
                "add top-level key",
                |d| d["surprise"] = json!(1),
                "top-level",
            ),
            (
                "add entry key",
                |d| d["request_config"]["tracked_flights"][0]["id"] = json!("abc"),
                "tracked_flights",
            ),
            (
                "drop entry key",
                |d| {
                    d["request_config"]["tracked_flights"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("created_at");
                },
                "tracked_flights",
            ),
        ];
        for (label, mutate, fragment) in cases {
            let mut document = response_document("get-configuration.json");
            mutate(&mut document);
            let transport = FakeTransport::scripted([ok(document)]);

            let snapshot = client(&transport).read();

            assert_eq!(
                snapshot.authority,
                SnapshotAuthority::NonAuthoritative,
                "{label}"
            );
            let reason = snapshot
                .reason
                .as_deref()
                .unwrap_or_else(|| panic!("{label}: no reason"));
            assert!(reason.contains(fragment), "{label}: {reason}");
            assert!(
                reason.starts_with("flightwall_contract_drift:"),
                "{label}: {reason}"
            );
        }
    }

    #[test]
    fn read_401_is_a_credential_error_that_never_echoes_the_key() {
        let transport = FakeTransport::scripted([invalid_key()]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("flightwall_credentials_rejected:1102")
        );
        assert!(!format!("{snapshot:?}").contains(credentials().api_key()));
    }

    #[test]
    fn read_cloudflare_403_is_fatal_not_retryable() {
        let transport = FakeTransport::scripted([Step::Reply(
            403,
            json!({"cloudflare_error": true, "error_code": 1010, "retryable": false}),
        )]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("flightwall_blocked:cloudflare_1010")
        );
    }

    #[test]
    fn read_other_statuses_are_named() {
        for (status, reason) in [
            (429, "flightwall_rate_limited"),
            (503, "flightwall_server_error:503"),
            (403, "flightwall_forbidden"),
            (418, "flightwall_unexpected_status:418"),
        ] {
            let transport = FakeTransport::scripted([Step::Reply(status, json!({}))]);

            let snapshot = client(&transport).read();

            assert_eq!(
                snapshot.authority,
                SnapshotAuthority::NonAuthoritative,
                "{status}"
            );
            assert_eq!(snapshot.reason.as_deref(), Some(reason), "{status}");
        }
    }

    #[test]
    fn read_transport_failure_is_non_authoritative() {
        let transport = FakeTransport::scripted([Step::Fail("connect")]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("flightwall_request_failed:connect")
        );
    }

    #[test]
    fn read_non_object_body_is_non_authoritative() {
        let transport = FakeTransport::scripted([ok(json!([1, 2, 3]))]);

        let snapshot = client(&transport).read();

        assert_eq!(
            snapshot.reason.as_deref(),
            Some("flightwall_response_not_object")
        );
    }

    // ---------------------------------------------------------------------------------------
    // Writes.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn replace_posts_the_whole_document_touching_only_tracked_flights() {
        let before = response_document("get-configuration.json");
        let mut expected = fixture_document("post-configuration-add.json", "request");
        let after = response_document("post-configuration-add.json");
        let transport = FakeTransport::scripted([ok(before), ok(after.clone()), ok(after)]);
        let wall = client(&transport);
        let snapshot = wall.read();

        let result = wall
            .replace_tracked_flights(
                &snapshot,
                &[
                    snapshot.tracked_flights[0].clone(),
                    TrackedFlight::new("BA5", now()),
                ],
            )
            .unwrap();

        assert_eq!(result.outcome, WriteOutcome::Applied);
        assert_eq!(result.snapshot.flight_numbers(), ["EI61", "BA5"]);
        let post = &transport.calls()[1];
        assert_eq!(
            (post.method, post.path.as_str()),
            (Method::Post, "/configuration")
        );
        assert_eq!(post.headers["content-type"], "application/json");
        let mut sent = post.body.clone().expect("a POST body");
        // Byte-for-byte the same as the app's own POST, apart from the values only the app knows.
        expected["userId"] = json!(credentials().user_id());
        let expected_flights = expected["request_config"]["tracked_flights"]
            .as_array()
            .unwrap()
            .clone();
        let sent_flights = sent["request_config"]["tracked_flights"]
            .as_array_mut()
            .unwrap();
        assert_eq!(sent_flights.len(), expected_flights.len());
        for (sent_flight, expected_flight) in sent_flights.iter_mut().zip(&expected_flights) {
            sent_flight["created_at"] = expected_flight["created_at"].clone();
        }
        assert_eq!(sent, expected);
        // And the daemon's re-read after the write is the third call.
        assert_eq!(
            transport.methods(),
            [Method::Get, Method::Post, Method::Get]
        );
    }

    #[test]
    fn replace_preserves_every_non_tracked_byte_of_the_document() {
        let before = response_document("get-configuration.json");
        let transport =
            FakeTransport::scripted([ok(before.clone()), ok(before.clone()), ok(before)]);
        let wall = client(&transport);
        let snapshot = wall.read();

        wall.replace_tracked_flights(&snapshot, &[]).unwrap();

        let sent = transport.calls()[1].body.clone().expect("a POST body");
        let original = response_document("get-configuration.json");
        for key in ["display_config", "version"] {
            assert_eq!(sent[key], original[key], "{key}");
        }
        for (key, value) in original["request_config"].as_object().unwrap() {
            if key != "tracked_flights" {
                assert_eq!(&sent["request_config"][key], value, "request_config.{key}");
            }
        }
        assert_eq!(sent["request_config"]["tracked_flights"], json!([]));
        assert!(sent.get("meta").is_none());
    }

    #[test]
    fn replace_refuses_more_than_five_before_any_request() {
        let transport = FakeTransport::scripted([ok(response_document("get-configuration.json"))]);
        let wall = client(&transport);
        let snapshot = wall.read();
        let six: Vec<TrackedFlight> = (1..=MAX_TRACKED_FLIGHTS + 1)
            .map(|i| TrackedFlight::new(format!("BA{i}"), now()))
            .collect();

        let refused = wall.replace_tracked_flights(&snapshot, &six).unwrap_err();

        assert_eq!(refused, WriteRefused::TooMany(6));
        assert!(refused.to_string().contains("at most 5"));
        assert_eq!(transport.calls().len(), 1);
    }

    #[test]
    fn replace_refuses_a_non_authoritative_snapshot() {
        let transport = FakeTransport::scripted([Step::Reply(503, json!({}))]);
        let wall = client(&transport);
        let snapshot = wall.read();

        let refused = wall.replace_tracked_flights(&snapshot, &[]).unwrap_err();

        assert_eq!(refused, WriteRefused::NonAuthoritative);
        assert_eq!(transport.calls().len(), 1);
    }

    #[test]
    fn replace_timeout_is_unknown_and_is_not_retried() {
        let before = response_document("get-configuration.json");
        let transport =
            FakeTransport::scripted([ok(before.clone()), Step::Fail("timeout"), ok(before)]);
        let wall = client(&transport);
        let snapshot = wall.read();

        let result = wall.replace_tracked_flights(&snapshot, &[]).unwrap();

        assert_eq!(result.outcome, WriteOutcome::Unknown);
        assert_eq!(
            result.reason.as_deref(),
            Some("flightwall_request_failed:timeout")
        );
        // Exactly one POST, then the re-read so the caller can compare.
        assert_eq!(
            transport.methods(),
            [Method::Get, Method::Post, Method::Get]
        );
        assert_eq!(result.snapshot.authority, SnapshotAuthority::Authoritative);
    }

    #[test]
    fn replace_rejected_status_is_rejected_with_no_re_read() {
        let before = response_document("get-configuration.json");
        let transport = FakeTransport::scripted([ok(before), invalid_key()]);
        let wall = client(&transport);
        let snapshot = wall.read();

        let result = wall.replace_tracked_flights(&snapshot, &[]).unwrap();

        assert_eq!(result.outcome, WriteOutcome::Rejected);
        assert_eq!(
            result.reason.as_deref(),
            Some("flightwall_credentials_rejected:1102")
        );
        assert_eq!(transport.methods(), [Method::Get, Method::Post]);
        // The caller gets back the snapshot it planned against, unchanged.
        assert_eq!(result.snapshot, snapshot);
    }

    #[test]
    fn wall_snapshot_debug_contains_no_document() {
        let failed = WallSnapshot::non_authoritative(now(), "flightwall_server_error:500");
        assert!(!format!("{failed:?}").contains("display_config"));

        let transport = FakeTransport::scripted([ok(response_document("get-configuration.json"))]);
        let live = client(&transport).read();
        let rendered = format!("{live:?}");
        assert!(!rendered.contains("display_config"));
        assert!(!rendered.contains("radius_request"));
        assert!(rendered.contains("EI61"));
    }
}
