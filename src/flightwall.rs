//! `FlightWall` client for the one contract the capture proved: a whole-document configuration.
//!
//! Everything here mirrors `docs/flightwall-api.md`. There is one resource,
//! `/configuration`; `GET` reads it and `POST` replaces it. Tracked flights are a list
//! inside it, keyed by `flight_number` and capped at five by the app, not the server. There
//! are no per-entry identifiers, no conditional writes, and no display mode, so
//! [`client::FlightWallClient`] offers exactly two operations and refuses anything the
//! captured contract did not show.

pub mod client;
pub mod contract;
pub mod credentials;
#[cfg(test)]
mod test_support;
pub mod transport;

pub use client::{FlightWallClient, WriteRefused, WriteResult};
pub use contract::{
    CONFIGURATION_PATH, FINGERPRINT_MODEL, Fingerprint, MAX_TRACKED_FLIGHTS, TrackedFlight,
    WallFailure, WallSnapshot, WriteOutcome,
};
pub use credentials::{CredentialsError, FlightWallCredentials, load_credentials};
pub use transport::{Headers, Method, Transport, TransportError, UreqTransport};
