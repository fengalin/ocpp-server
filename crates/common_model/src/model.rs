//! A timestamp on the wire, as **milliseconds since the Unix epoch**.
//!
//! We store/serialize epoch millis (not ISO-8601 strings) for two reasons:
//!   1. it is the only time representation that can cross the `wasm-bindgen`
//!      boundary (chrono types cannot), so the Rust frontend shares this exact
//!      type with the server;
//!   2. it is far smaller on the wire than an ISO string, which matters for the
//!      per-sample SSE stream and the unix-socket notification stream.
//!
//! All plan/schedule *computation* still uses `chrono::DateTime<Utc>`; only the
//! serialization boundary converts through this newtype.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::notification::ChargeStatus;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Ts(pub i64);

// [frontend] `Ts` is epoch *milliseconds* (transparent i64) on the wire, used
// by every endpoint + SSE event and by the unix-socket notifications. The WASM
// frontend renders local time from it via `Date` (web-app/src/state.rs) and
// parses `datetime-local` / ISO-8601 inputs back into it. If the granularity or
// base ever changes, the frontend `fmt_time` / `to_iso_utc` / `from_iso`
// helpers must change with it.

impl Ts {
    pub const fn from_utc(ts: DateTime<Utc>) -> Self {
        Self(ts.timestamp_millis())
    }

    pub fn to_utc(self) -> DateTime<Utc> {
        // Values read from the wire are untrusted: clamp out-of-range millis
        // to chrono's representable range instead of panicking (a corrupt
        // payload used to kill the process via `expect`).
        DateTime::from_timestamp_millis(self.0.clamp(
            DateTime::<Utc>::MIN_UTC.timestamp_millis(),
            DateTime::<Utc>::MAX_UTC.timestamp_millis(),
        ))
        .expect("clamped value is representable")
    }
}

impl From<DateTime<Utc>> for Ts {
    fn from(ts: DateTime<Utc>) -> Self {
        Self::from_utc(ts)
    }
}

impl From<Ts> for DateTime<Utc> {
    fn from(ts: Ts) -> Self {
        ts.to_utc()
    }
}

impl std::ops::Add<Duration> for Ts {
    type Output = Ts;
    fn add(self, rhs: Duration) -> Self::Output {
        // Saturate instead of wrapping on overflow.
        Ts(self.0.saturating_add(rhs.num_milliseconds()))
    }
}

// ---------------------------------------------------------------------
// Event log
//
// `EventLogEntry` is the status-change record both the web server and the
// front end carry in their event log. The status is the charge point's
// `ChargeStatus` (see the `notification` module).
// ---------------------------------------------------------------------

/// A status-change event, as kept in the server's / front end's event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventLogEntry {
    pub ts: Ts,
    pub status: ChargeStatus,
}
