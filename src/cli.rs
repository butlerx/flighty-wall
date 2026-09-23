//! Command-line entry points for safe setup and diagnostics.
//!
//! Every command loads the same config, builds the same clients, and drives the same
//! engine as the daemon, so what `sync` prints is exactly what `run` would do.

use crate::{
    calendar::{
        AuthError, CalendarLimits, CalendarReader, GoogleCalendarGateway,
        ServiceAccountTokenSource, UreqJsonHttp, sanitize_event_payload,
    },
    capture::{CaptureError, manifest, observed_hosts, sanitize_har},
    config::{AppConfig, ConfigError, load_config},
    flightwall::{CredentialsError, FlightWallClient, UreqTransport, load_credentials},
    service::{
        CycleError, CycleReport, HostLock, LockError, StopSignal, install_stop_signals,
        lock_path_for, run_cycle, run_forever,
    },
    state::{StateError, StateStore},
};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Inspection reads are diagnostic only, so they may look wider than the daemon window.
pub const MAX_INSPECTION_WINDOW_DAYS: u32 = 365;

/// A source could not be read authoritatively; nothing was written.
pub const EXIT_NOT_AUTHORITATIVE: u8 = 3;
/// The wall rejected the write or its outcome is unknown; see the summary line.
pub const EXIT_WRITE_FAILED: u8 = 4;
/// Another mutating process holds the host lock.
pub const EXIT_BUSY: u8 = 5;
/// Configuration or environment problem; nothing was attempted.
pub const EXIT_CONFIG: u8 = 2;

type Reader =
    CalendarReader<GoogleCalendarGateway<UreqJsonHttp, ServiceAccountTokenSource<UreqJsonHttp>>>;
type Wall = FlightWallClient<UreqTransport>;

/// Sync Flighty Friends' flights to a `FlightWall`.
#[derive(Debug, Parser)]
#[command(name = "flighty-wall", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Read the configured calendar and write a sanitized fixture.
    InspectCalendar {
        #[arg(long = "config", value_name = "PATH")]
        config_path: PathBuf,
        #[arg(long = "output", value_name = "PATH")]
        output_path: PathBuf,
        /// Name or other literal text to replace in the fixture; repeat as needed.
        #[arg(long = "redact-term", value_name = "TEXT")]
        sensitive_terms: Vec<String>,
        /// Read this many days ahead instead of `service.lookahead_days`.
        #[arg(long, value_parser = clap::value_parser!(u32).range(0..=MAX_INSPECTION_WINDOW_DAYS as i64))]
        lookahead_days: Option<u32>,
        /// Also read this many days of past events.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=MAX_INSPECTION_WINDOW_DAYS as i64))]
        lookback_days: u32,
    },
    /// Turn a HAR capture of your own `FlightWall` app traffic into committable fixtures.
    SanitizeCapture {
        /// HAR file exported by the proxy.
        #[arg(long = "input", value_name = "PATH")]
        input_path: PathBuf,
        #[arg(long, value_name = "DIR")]
        output_dir: PathBuf,
        /// Keep only entries for this host; repeat as needed, omit to keep every host.
        #[arg(long = "host", value_name = "HOST")]
        hosts: Vec<String>,
        /// Name, device label, or other literal text to replace; repeat as needed.
        #[arg(long = "redact-term", value_name = "TEXT")]
        sensitive_terms: Vec<String>,
    },
    /// Read the wall's configuration once and report the tracked flights. Never writes.
    ProbeWall {
        #[arg(long = "config", value_name = "PATH")]
        config_path: PathBuf,
    },
    /// Run one cycle and print what it did or would do.
    Sync {
        #[arg(long = "config", value_name = "PATH")]
        config_path: PathBuf,
        /// Write to the wall even if `service.dry_run` is true; without it this is a dry run.
        #[arg(long = "apply")]
        apply_flag: bool,
    },
    /// Run cycles on the configured interval until SIGTERM or SIGINT.
    Run {
        #[arg(long = "config", value_name = "PATH")]
        config_path: PathBuf,
        /// DEBUG, INFO, WARNING, or ERROR.
        #[arg(long, default_value = "INFO")]
        log_level: String,
    },
}

/// Why a command stopped before or during its work, and the exit code that means.
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),
    #[error("configuration error: {0}")]
    Credentials(#[from] CredentialsError),
    #[error("configuration error: {0}")]
    Auth(#[from] AuthError),
    #[error("configuration error: {0}")]
    Message(String),
    #[error("busy: {0}")]
    Lock(#[from] LockError),
    #[error("state error: {0}")]
    State(#[from] StateError),
    #[error("cycle error: {0}")]
    Cycle(#[from] CycleError),
    #[error("capture error: {0}")]
    Capture(#[from] CaptureError),
    #[error("{0}")]
    NotAuthoritative(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Failure {
    /// The process exit code this failure maps to.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Lock(_) => EXIT_BUSY,
            Self::Config(_)
            | Self::Credentials(_)
            | Self::Auth(_)
            | Self::Message(_)
            | Self::State(_)
            | Self::Cycle(_)
            | Self::Capture(_)
            | Self::NotAuthoritative(_)
            | Self::Io { .. } => EXIT_CONFIG,
        }
    }
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Failure {
    move |source| Failure::Io {
        context: context.into(),
        source,
    }
}

/// Run one parsed command. `Ok(code)` is a completed command with that exit code.
///
/// # Errors
///
/// [`Failure`] when the command could not complete; the message goes to stderr and
/// [`Failure::exit_code`] to the shell.
pub fn run(cli: Cli) -> Result<u8, Failure> {
    match cli.command {
        Command::InspectCalendar {
            config_path,
            output_path,
            sensitive_terms,
            lookahead_days,
            lookback_days,
        } => inspect_calendar(
            &config_path,
            &output_path,
            &sensitive_terms,
            lookahead_days,
            lookback_days,
        ),
        Command::SanitizeCapture {
            input_path,
            output_dir,
            hosts,
            sensitive_terms,
        } => sanitize_capture(&input_path, &output_dir, &hosts, &sensitive_terms),
        Command::ProbeWall { config_path } => probe_wall(&config_path),
        Command::Sync {
            config_path,
            apply_flag,
        } => sync(&config_path, apply_flag),
        Command::Run {
            config_path,
            log_level,
        } => run_daemon(&config_path, &log_level),
    }
}

fn inspect_calendar(
    config_path: &Path,
    output_path: &Path,
    sensitive_terms: &[String],
    lookahead_days: Option<u32>,
    lookback_days: u32,
) -> Result<u8, Failure> {
    let config = load_config(config_path)?;
    let reader = build_reader(&config, lookahead_days, lookback_days)?;

    let snapshot = reader.read_snapshot(Utc::now());
    if !snapshot.is_authoritative() {
        return Err(Failure::NotAuthoritative(format!(
            "calendar inspection failed: {}",
            snapshot.reason.as_deref().unwrap_or("unknown")
        )));
    }

    let terms: Vec<&str> = sensitive_terms.iter().map(String::as_str).collect();
    let payload = json!({
        "authority": snapshot.authority.as_str(),
        "captured_at": rfc3339(snapshot.observed_at),
        "events": snapshot.events.iter().map(|event| {
            let fields: serde_json::Map<String, Value> = event.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            Value::Object(sanitize_event_payload(&fields, &terms))
        }).collect::<Vec<_>>(),
    });
    atomic_private_json(output_path, &payload)?;
    println!(
        "wrote {} sanitized event(s) to {}",
        snapshot.events.len(),
        output_path.display()
    );
    Ok(0)
}

fn sanitize_capture(
    input_path: &Path,
    output_dir: &Path,
    hosts: &[String],
    sensitive_terms: &[String],
) -> Result<u8, Failure> {
    let bytes = fs::read(input_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Failure::Message(format!(
                "capture file does not exist: {}",
                input_path.display()
            ))
        } else {
            io(format!("cannot read {}", input_path.display()))(error)
        }
    })?;
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|error| Failure::Message(format!("capture file is not valid JSON: {error}")))?;
    if !document.is_object() {
        return Err(Failure::Message(
            "capture file is not a HAR archive: top level is not an object".to_owned(),
        ));
    }

    let terms: Vec<&str> = sensitive_terms.iter().map(String::as_str).collect();
    let host_refs: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let entries = sanitize_har(&document, &terms, &host_refs)?;
    let every_host = observed_hosts(&document)?;

    if entries.is_empty() {
        let listed = if every_host.is_empty() {
            "none".to_owned()
        } else {
            every_host.join(", ")
        };
        println!("no entries matched. hosts in this capture: {listed}");
        return Ok(1);
    }

    for entry in &entries {
        let filename = entry.filename();
        atomic_private_json(&output_dir.join(&filename), &entry.payload)?;
        println!(
            "{:<6} {:<4} {}{} -> {filename}",
            entry.method, entry.status, entry.host, entry.path
        );
    }
    atomic_private_json(
        &output_dir.join("manifest.json"),
        &manifest(&entries, &terms),
    )?;
    println!(
        "wrote {} sanitized entr(ies) and manifest.json to {}",
        entries.len(),
        output_dir.display()
    );
    println!("review every file by hand before committing, then delete the raw capture");
    Ok(0)
}

fn probe_wall(config_path: &Path) -> Result<u8, Failure> {
    let config = load_config(config_path)?;
    let wall = build_wall(&config)?;

    let snapshot = wall.read();
    if !snapshot.is_authoritative() {
        return Err(Failure::NotAuthoritative(format!(
            "wall read failed: {}",
            snapshot.reason.as_deref().unwrap_or("unknown")
        )));
    }

    println!("observed_at: {}", rfc3339(snapshot.observed_at));
    println!(
        "model: {}",
        snapshot.fingerprint.model.as_deref().unwrap_or("None")
    );
    println!("tracked_flights: {}", snapshot.tracked_flights.len());
    for flight in &snapshot.tracked_flights {
        println!("  {}  added {}", flight.flight_number, flight.created_at);
    }
    Ok(0)
}

fn sync(config_path: &Path, apply_flag: bool) -> Result<u8, Failure> {
    let config = load_config(config_path)?;
    let wall = build_wall(&config)?;
    let calendar = build_reader(&config, None, 0)?;

    let apply = apply_flag || !config.service.dry_run;
    let _lock = HostLock::acquire(&lock_path_for(&config.storage.state_path))?;
    let store = StateStore::open(&config.storage.state_path)?;
    let report = run_cycle(&calendar, &wall, &store, Utc::now(), apply)?;

    print_report(&report, apply);
    Ok(exit_code(&report))
}

fn run_daemon(config_path: &Path, log_level: &str) -> Result<u8, Failure> {
    configure_logging(log_level);
    let config = load_config(config_path)?;
    let wall = build_wall(&config)?;
    let calendar = build_reader(&config, None, 0)?;

    let apply = !config.service.dry_run;
    let stop = StopSignal::new();
    install_stop_signals(&stop).map_err(io("cannot install signal handlers"))?;
    let _lock = HostLock::acquire(&lock_path_for(&config.storage.state_path))?;
    let store = StateStore::open(&config.storage.state_path)?;

    eprintln!(
        "flighty-wall starting: interval={}s lookahead={}d mode={}",
        config.service.poll_interval_seconds,
        config.service.lookahead_days,
        if apply { "apply" } else { "dry-run" }
    );
    let interval =
        Duration::from_secs(u64::try_from(config.service.poll_interval_seconds).unwrap_or(120));
    let epoch = Instant::now();
    let cycles = run_forever(
        || run_cycle(&calendar, &wall, &store, Utc::now(), apply),
        interval,
        &stop,
        || epoch.elapsed(),
        |timeout| stop.wait(timeout),
    );
    eprintln!("flighty-wall stopped after {cycles} cycle(s)");
    Ok(0)
}

/// Plain stderr lines for the journal: systemd adds the timestamp and unit.
fn configure_logging(level: &str) {
    let level = match level.to_ascii_uppercase().as_str() {
        "DEBUG" => log::LevelFilter::Debug,
        "WARNING" | "WARN" => log::LevelFilter::Warn,
        "ERROR" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };
    let _ = env_logger::Builder::new()
        .filter_level(level)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} {}: {}",
                record.level(),
                record.target(),
                record.args()
            )
        })
        .try_init();
}

fn print_report(report: &CycleReport, apply: bool) {
    println!("{}", report.summary());
    if report
        .plan
        .as_ref()
        .is_some_and(super::reconcile::Plan::changed)
        && !apply
    {
        println!(
            "dry run: nothing was written. Re-run with --apply, or set service.dry_run = false."
        );
    }
}

fn exit_code(report: &CycleReport) -> u8 {
    if report.status.is_non_authoritative() {
        EXIT_NOT_AUTHORITATIVE
    } else if report.status.is_write_failure() {
        EXIT_WRITE_FAILED
    } else {
        0
    }
}

fn build_wall(config: &AppConfig) -> Result<Wall, Failure> {
    let settings = config.flightwall.as_ref().ok_or_else(|| {
        Failure::Message(
            "no [flightwall] table: add one before probing or syncing the wall".to_owned(),
        )
    })?;
    let credentials = load_credentials(&settings.credentials_path)?;
    let transport = UreqTransport::new(
        &settings.host,
        Duration::from_secs_f64(settings.timeout_seconds),
    );
    Ok(FlightWallClient::new(
        transport,
        credentials,
        &settings.user_agent,
    ))
}

fn build_reader(
    config: &AppConfig,
    lookahead_days: Option<u32>,
    lookback_days: u32,
) -> Result<Reader, Failure> {
    let timeout = Duration::from_secs(30);
    let tokens = ServiceAccountTokenSource::from_key_file(
        &config.google.credentials_path,
        UreqJsonHttp::new(timeout),
    )?;
    let gateway = GoogleCalendarGateway::new(UreqJsonHttp::new(timeout), tokens);
    let configured = u32::try_from(config.service.lookahead_days).unwrap_or(7);
    Ok(CalendarReader::new(
        gateway,
        config.google.calendar_id.clone(),
        lookahead_days.unwrap_or(configured),
        lookback_days,
        CalendarLimits::from(&config.calendar_limits),
    ))
}

/// Write `payload` as indented, key-sorted JSON via a 0600 temp file and an atomic rename.
fn atomic_private_json(path: &Path, payload: &Value) -> Result<(), Failure> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(io(format!("cannot create {}", parent.display())))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(
            ".{}.",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        ))
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(io(format!(
            "cannot create temp file in {}",
            parent.display()
        )))?;
    let context = format!("cannot write {}", path.display());
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o600))
        .map_err(io(context.clone()))?;
    let mut encoded =
        serde_json::to_vec_pretty(payload).map_err(|e| Failure::Message(e.to_string()))?;
    encoded.push(b'\n');
    temporary.write_all(&encoded).map_err(io(context.clone()))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(io(context.clone()))?;
    temporary
        .persist(path)
        .map_err(|e| io(context.clone())(e.error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io(context))?;
    Ok(())
}

fn rfc3339(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}
