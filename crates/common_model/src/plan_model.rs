//! Data types for the scheduling / charging-plan model.
//!
//! Shared by the server, the WASM web front end and the web mock: the plan
//! itself (`Plan`, `ChargingPeriod`), its preview (`PlanPreview`) and the
//! live-progress view (`LivePlan`). The *computation* over these types lives
//! in [`crate::plan`].

use chrono::{DateTime, Duration, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bms::Bms;
use crate::model::Ts;

/// One consecutive block of a charging plan. Periods with `power_limit == 0`
/// (W) are intentional gaps (future TOU pause behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargingPeriod {
    pub start_at: Ts,
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
    /// Block charging (permanent 0 W plan).
    Blocking,
    /// No plan in effect (no restriction on charging power).
    Unlimited,
}

/// The EVSE / CLI charging-plan model: what the server applies to the
/// charging point (OCPP `SetChargingProfile`) and what the web app requests /
/// is notified of (`notification::Notification::ChargingPlan`). Shared by the
/// server, the WASM web front end and the OLED status app.
///
/// The OCPP schedule computation for a plan lives in the server crate
/// (`schedule::ChargingSchedule::try_from_charging_plan`) — it builds the
/// server-local `ChargingSchedule` type.
#[derive(Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChargingPlan {
    /// Permanent 0 W plan.
    Blocking,
    OffPeakToday {
        power_limit: u32,
    },
    OffPeakTomorrow {
        power_limit: u32,
    },
    /// Reach `target_soc` (*fraction* 0–1) by the local time of day
    /// `deadline`.
    Deadline {
        deadline: NaiveTime,
        target_soc: f32,
        power_limit: u32,
    },
    /// Start now, stop at `target_soc` (*fraction* 0–1).
    Asap {
        target_soc: f32,
        power_limit: u32,
    },
    /// No plan in effect (no restriction on charging power).
    Unlimited,
}

impl ChargingPlan {
    /// The target SoC (*fraction* 0–1) this plan charges up to, if it has one.
    pub fn target_soc(&self) -> Option<f32> {
        match self {
            Self::Deadline { target_soc, .. } | Self::Asap { target_soc, .. } => Some(*target_soc),
            _ => None,
        }
    }
}

/// A charging plan as applied on the EVSE: the requested [`ChargingPlan`]
/// plus the schedule it computed — the periods the charging point will
/// actually follow (empty for `Unlimited` and for `Blocking` — a permanent 0 W
/// plan, which is also what gets applied when a plan's schedule could not be
/// computed). The payload of
/// [`crate::Notification::ChargingPlan`] and what the server reports as the
/// plan in effect (`ServerState`); shared by the server, the WASM web front
/// end and the OLED status app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedChargingPlan {
    pub plan: ChargingPlan,
    /// The computed schedule (empty when the plan has no schedule).
    pub periods: Vec<ChargingPeriod>,
}

impl AppliedChargingPlan {
    /// End of the last period, if any.
    pub fn end(&self) -> Option<Ts> {
        self.periods
            .last()
            .map(|p| Ts(p.start_at.0 + (p.duration as i64) * 1_000))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanState {
    Active,
    Completed,
    Cancelled,
}

/// The computed preview of a plan; applying the plan stores these periods
/// verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub energy: u64,
    pub soc_delta: f32,
    pub soc_result: f32,
    pub duration: u64,
    pub completes_at: Ts,
    pub feasible: bool,
    /// Average power received by the battery, in W.
    pub avg_battery: u32,
    pub periods: Vec<ChargingPeriod>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub kind: PlanKind,
    pub start_at: Ts,
    /// *Fraction* 0–1 (see [`Bms`]); the frontend renders it `* 100`.
    pub target_soc: f32,
    pub deadline: Option<Ts>,
    pub initial_soc: f32,
    pub periods: Vec<ChargingPeriod>,
    pub state: PlanState,
    pub preview: PlanPreview,
    pub created_ts: Ts,
}

impl Plan {
    /// End of the last period.
    pub fn end(&self) -> DateTime<Utc> {
        self.periods
            .iter()
            .map(|p| p.start_at.to_utc() + Duration::seconds(p.duration as i64))
            .max()
            .unwrap_or(self.start_at.to_utc())
    }

    /// Power limit in W at `ts`, or 0 when no period covers `ts`.
    pub fn limit_at(&self, ts: DateTime<Utc>) -> u32 {
        self.periods
            .iter()
            .find(|p| {
                let start = p.start_at.to_utc();
                ts >= start && ts < start + Duration::seconds(p.duration as i64)
            })
            .map(|p| p.power_limit)
            .unwrap_or(0)
    }
}

/// Battery energy (Wh) needed to go from `from_soc` to `to_soc` (0 if not).
///
/// SoC arguments are **fractions 0–1** (same convention as `Plan.target_soc`,
/// `Bms.current_soc`), so the delta scales the capacity
/// directly — no percent conversion.
pub fn battery_energy_needed(from_soc: f32, to_soc: f32, bms: &Bms) -> f64 {
    let soc_delta = (to_soc - from_soc).max(0.0);
    let energy = soc_delta * bms.capacity as f32;
    energy as f64
}

/// Snapshot of the active plan with live progress, as shown on the status tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LivePlan {
    pub plan: Plan,
    pub remaining_energy: u64,
    pub remaining_time: u64,
    pub eta: Ts,
}
