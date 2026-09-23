-- The wall carries no ownership signal (docs/flightwall-api.md §4), so this journal is the
-- only record of which flight_number entries the daemon added. A flight_number absent from
-- owned_flights is the owner's and is never removed.
--
-- IF NOT EXISTS throughout: a state database left behind by an earlier build of the daemon
-- already holds these tables (plus its own schema_info), and must open in place with its
-- rows intact rather than fail on the first CREATE.

CREATE TABLE IF NOT EXISTS metadata (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS owned_flights (
    flight_number  TEXT PRIMARY KEY,
    first_added_at TEXT NOT NULL,
    source_key     TEXT NOT NULL
);

-- One row at most: the desired list the daemon was about to POST, and what the wall held
-- just before, so an interrupted write can be settled from a re-read on the next start.
CREATE TABLE IF NOT EXISTS pending_writes (
    singleton  INTEGER PRIMARY KEY CHECK (singleton = 1),
    desired    TEXT NOT NULL,
    "before"   TEXT NOT NULL DEFAULT '[]',
    started_at TEXT NOT NULL
);
