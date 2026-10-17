//! Data model. Mirrors the structs the real OCPP server will expose, so the
//! simulator in this draft can be swapped for the real data source later.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Worst-case sample history kept in memory (~7 days at 10 s cadence).
pub const MAX_METER_VALUES: usize = 65_000;
/// Maximum kept status-change events.
pub const MAX_EVENTS: usize = 1_000;

/// Charge point status (as reported by the user's server).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Available,
    Preparing,
    Charging,
    SuspendedByEVSE,
    Finished,
    Rebooting,
    Faulted,
}

/// An ongoing (or just finished) charging session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub start_ts: DateTime<Utc>,
    pub status: Status,
    pub initial_energy: u64,
    pub initial_soc: f32,
    pub cur_soc: f32,
}

/// One consecutive block of a charging plan. Periods with `power_limit == 0`
/// (W) are intentional gaps (future TOU pause behaviour).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ChargingPeriod {
    pub start_at: DateTime<Utc>,
    pub duration: u64,
    pub power_limit: u32,
}

// FIXME remove
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanKind {
    /// Must reach the target SoC by `deadline`.
    Deadline,
    /// Start as soon as possible, stop at the target SoC.
    Asap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanState {
    Active,
    Completed,
    Cancelled,
}

/// What a plan will do, computed before it is applied. The UI shows exactly
/// this; applying the plan stores these periods verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanPreview {
    pub energy: u64,
    pub soc_delta: f32,
    pub soc_result: f32,
    pub duration: u64,
    pub completes_at: DateTime<Utc>,
    pub feasible: bool,
    /// Average power received by the battery, in W.
    pub avg_battery: u32,
    pub periods: Vec<ChargingPeriod>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub kind: PlanKind,
    pub start_at: DateTime<Utc>,
    pub target_soc: f32,
    pub deadline: Option<DateTime<Utc>>,
    pub initial_soc: f32,
    pub periods: Vec<ChargingPeriod>,
    pub state: PlanState,
    pub preview: PlanPreview,
    pub created_ts: DateTime<Utc>,
}

impl Plan {
    /// End of the last period.
    pub fn end(&self) -> DateTime<Utc> {
        self.periods
            .iter()
            .map(|p| p.start_at + chrono::Duration::seconds(p.duration as i64))
            .max()
            .unwrap_or(self.start_at)
    }

    /// Power limit in W at `ts`, or 0 when no period covers `ts`.
    pub fn limit_at(&self, ts: DateTime<Utc>) -> u32 {
        self.periods
            .iter()
            .find(|p| {
                ts >= p.start_at && ts < p.start_at + chrono::Duration::seconds(p.duration as i64)
            })
            .map(|p| p.power_limit)
            .unwrap_or(0)
    }
}

/// Fixed characteristics of the real installation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Characteristics {
    /// Battery capacity in Wh.
    pub battery_capacity: u64,
    pub max_power: u32,
    pub static_loss: u32,
}

impl Default for Characteristics {
    fn default() -> Self {
        Self {
            battery_capacity: 48_100,
            max_power: 7400,
            static_loss: 400,
        }
    }
}

/// Battery energy (Wh) needed to go from `from_soc` to `to_soc` (0 if not).
pub fn battery_energy_needed(from_soc: f32, to_soc: f32, ch: &Characteristics) -> f64 {
    ((to_soc - from_soc) as f64 / 100.0 * ch.battery_capacity as f64).max(0.0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogEntry {
    pub ts: DateTime<Utc>,
    pub status: Status,
    pub note: Option<String>,
}

/// Request body for `/api/plans/preview` and `/api/plans`.
#[derive(Debug, Clone, Deserialize)]
pub struct PlanRequest {
    pub kind: PlanKind,
    pub target_soc: f32,
    pub deadline: Option<DateTime<Utc>>,
}

/// Snapshot of the active plan with live progress, as shown on the status tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivePlan {
    pub plan: Plan,
    pub remaining_energy: u64,
    pub remaining_time: u64,
    pub eta: DateTime<Utc>,
}

/// Full state returned by `GET /api/state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerState {
    pub ts: DateTime<Utc>,
    pub characteristics: Characteristics,
    pub status: Status,
    pub soc: f32,
    pub session: Option<Session>,
    pub active_plan: Option<LivePlan>,
}

/// Events pushed over `GET /api/stream` (SSE).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t")]
pub enum StreamEvent {
    MeterValues {
        meter_values: crate::measurements::MeterValueSelection,
        soc: f32,
    },
    Status {
        status: Status,
        session: Option<Session>,
    },
    Plan,
}

impl StreamEvent {
    pub fn name(&self) -> &str {
        use StreamEvent::*;
        match self {
            MeterValues { .. } => "meter_values",
            Status { .. } => "status",
            Plan => "plan",
        }
    }
}
