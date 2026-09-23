//! Sync Flighty Friends flights from a dedicated Google Calendar to a `FlightWall` Mini.

pub mod calendar;
pub mod capture;
pub mod cli;
pub mod config;
pub mod flightwall;
pub mod models;
pub mod parser;
pub mod reconcile;
pub mod redaction;
pub mod service;
pub mod state;
