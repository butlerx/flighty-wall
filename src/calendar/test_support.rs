//! Fakes and fixtures shared by the calendar submodules' tests.

use std::cell::RefCell;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};

use super::google::{JsonHttp, TokenSource};
use crate::flightwall::TransportError;

pub(crate) fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}

pub(crate) fn now() -> DateTime<Utc> {
    utc(2026, 9, 21, 12, 0)
}

pub(crate) fn timed_event(event_id: &str) -> Value {
    timed_event_with(event_id, "AA123 · Friend", "2026-09-22T08:00:00-04:00")
}

pub(crate) fn timed_event_with(event_id: &str, summary: &str, start: &str) -> Value {
    json!({
        "id": event_id,
        "status": "confirmed",
        "summary": summary,
        "description": "Flight AA123 from JFK to ORD",
        "start": {"dateTime": start, "timeZone": "America/New_York"},
        "end": {"dateTime": "2026-09-22T10:30:00-05:00", "timeZone": "America/Chicago"},
        "updated": "2026-09-21T11:00:00Z",
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpCall {
    Get {
        url: String,
        query: Vec<(String, String)>,
        bearer: String,
    },
    PostForm {
        url: String,
        form: Vec<(String, String)>,
    },
}

pub(crate) struct FakeHttp {
    pub(crate) get_responses: RefCell<Vec<(u16, Value)>>,
    post_responses: RefCell<Vec<(u16, Value)>>,
    calls: RefCell<Vec<HttpCall>>,
}

impl FakeHttp {
    pub(crate) fn new(gets: Vec<(u16, Value)>, posts: Vec<(u16, Value)>) -> Self {
        Self {
            get_responses: RefCell::new(gets.into_iter().rev().collect()),
            post_responses: RefCell::new(posts.into_iter().rev().collect()),
            calls: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn calls(&self) -> Vec<HttpCall> {
        self.calls.borrow().clone()
    }
}

pub(crate) fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

impl JsonHttp for FakeHttp {
    fn get_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
        bearer: &str,
    ) -> Result<(u16, Value), TransportError> {
        self.calls.borrow_mut().push(HttpCall::Get {
            url: url.to_owned(),
            query: owned(query),
            bearer: bearer.to_owned(),
        });
        self.get_responses
            .borrow_mut()
            .pop()
            .ok_or_else(|| TransportError::new("unscripted_get"))
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), TransportError> {
        self.calls.borrow_mut().push(HttpCall::PostForm {
            url: url.to_owned(),
            form: owned(form),
        });
        self.post_responses
            .borrow_mut()
            .pop()
            .ok_or_else(|| TransportError::new("unscripted_post"))
    }
}

pub(crate) struct FixedToken(pub(crate) &'static str);

impl TokenSource for FixedToken {
    fn bearer_token(&self) -> Result<String, String> {
        Ok(self.0.to_owned())
    }
}

pub(crate) struct NoToken;

impl TokenSource for NoToken {
    fn bearer_token(&self) -> Result<String, String> {
        Err("http_401".to_owned())
    }
}
