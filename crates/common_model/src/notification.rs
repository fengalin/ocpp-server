//! local clients (the external OLED status app).
//!
//! These types are serialized to JSON and written to [`UNIX_SOCKET_PATH`] by
//! the server's `UnixSocketNotifier`. The OLED app is a client of that socket —
//! it does not speak OCPP — so the exact enum shapes here are the API the
//! OLED app deserializes. Keep them stable.

use crate::bms::Bms;
use crate::meter_value::MeterValueSelection;
use crate::model::Ts;
use crate::plan_model::AppliedChargingPlan;

pub const UNIX_SOCKET_PATH: &str = "/run/charge_system/socket";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Notification {
    BmsUpdate {
        ts: Ts,
        bms: Bms,
    },
    Status {
        ts: Ts,
        status: ChargeStatus,
    },
    Connection {
        ts: Ts,
        state: ChargeSystemConnectionState,
    },
    MeterValues(MeterValueSelection),
    /// A plan was applied on the EVSE (including the plan-less `Unlimited` and
    /// the default post-session `Blocking`, both with no periods): the
    /// requested plan + the schedule it computed.
    ChargingPlan(AppliedChargingPlan),
}

impl Notification {
    /// The SSE event name for this notification (the web server's
    /// `/api/stream` emits `Notification`s directly under these names).
    pub fn name(&self) -> &str {
        match self {
            Notification::BmsUpdate { .. } => "bms_update",
            Notification::Status { .. } => "status",
            Notification::Connection { .. } => "connection",
            Notification::MeterValues(_) => "meter_values",
            Notification::ChargingPlan(_) => "charging_plan",
        }
    }

    /// JSON encoding of this notification, for the SSE `data:` field (SSE
    /// payloads are text, so it is serialized straight to a JSON string).
    pub fn to_sse_data(&self) -> String {
        // Infallible for these shapes; never panic per event per connection —
        // a failure would only mean a broken SSE frame, which the client
        // reports and skips.
        serde_json::to_string(self).unwrap_or_else(|_| String::from("null"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ChargeStatus {
    Available,
    Preparing,
    Charging,
    SuspendedEvse,
    SoCTargetReached,
    SuspendedEv,
    StoppedByUser,
    Finishing,
    Unknown,
    Error,
}

impl ChargeStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            ChargeStatus::Preparing | ChargeStatus::Charging | ChargeStatus::SuspendedEvse
        )
    }
}

#[derive(Debug, Copy, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ChargeSystemConnectionState {
    Heartbeat,
    MissedHeartbeat,
    ServerDisconnected,
    Error,
}
