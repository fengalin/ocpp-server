//! Data model for the web/HTTP+SSE wire contract.
//!
//! **Only types pertinent to the web app itself are defined here**
//! (`ServerState`, `PlanRequest`, ring limits). Everything shared with the
//! server's other consumers — `Ts`, `MeterValueSelection`, the status /
//! event-log types, the whole scheduling/charging-plan model plus its
//! computation, and the notification stream itself (the SSE events of
//! `GET /api/stream` are `charge_system_common_model::notification::Notification`s) —
//! lives in `charge-system-common-model` and is used directly from there
//! (no re-exports).

use serde::{Deserialize, Serialize};

use charge_system_common_model::{
    bms::Bms,
    model::Ts,
    notification::ChargeStatus,
    plan_model::{AppliedChargingPlan, PlanKind},
};

/// Metric values kept in memory (a few days at the default ~150 s cadence).
pub const MAX_METER_VALUES: usize = 2048;
/// Status-change entries kept in the event log.
pub const MAX_EVENTS: usize = 32;

/// Request body for `/api/plans/apply`.
///
/// `Serialize` is needed by the WASM front end (it builds the request); the
/// server only ever deserializes it.
///
/// `kind` selects the plan: `Deadline` / `Asap` carry their options below,
/// while `Blocking` and `Unlimited` have none (the fields are `None`).
///
/// `target_soc` is a *fraction* 0–1 (see [`Plan`]); the form's percent value
/// is divided by 100 before sending (web-app/src/ui).
///
/// `initial_soc` is the *last known* state of charge at the start of the
/// session (fraction 0–1), as set by the init-SoC widget — it is the seed for
/// the server's SoC re-estimation (`plan::estimate_soc`), so a plan applied
/// with a stale/unknown SoC is meaningless. `None` when the widget has never
/// been set.
///
/// `power_limit` is the user-chosen grid power limit in W for the plan's
/// charging period(s), as set by the power-limit widget in the plan form.
/// It is clamped to the CP's `max_power` by `plan::compute`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)] // catch typos / stale clients at the boundary
pub struct PlanRequest {
    pub kind: PlanKind,
    #[serde(default)]
    pub target_soc: Option<f32>,
    #[serde(default)]
    pub deadline: Option<Ts>,
    #[serde(default)]
    pub initial_soc: Option<f32>,
    #[serde(default)]
    pub power_limit: Option<u32>,
}

/// Full state returned by `GET /api/state`.
///
/// [frontend] `soc` is a *fraction* 0–1 (rendered `* 100` by the frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)] // catch version skew between server and front end
pub struct ServerState {
    pub ts: Ts,
    pub bms: Bms,
    pub status: ChargeStatus,
    /// The plan currently in effect on the EVSE (or `None`), with its
    /// computed schedule, as notified by the server
    /// (`Notification::ChargingPlan`).
    pub charging_plan: Option<AppliedChargingPlan>,
}
