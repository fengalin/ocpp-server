//! Data types for the scheduling / charging-plan model.
//!
//! Shared by the server, the WASM web front end and the web mock: the plan
//! itself (`Plan`, `ChargingPeriod`), its preview (`PlanPreview`) and the
//! live-progress view (`LivePlan`). The *computation* over these types lives
//! in [`crate::plan`].

use std::fmt::{self, Write};
use std::str::FromStr;

#[cfg(feature = "server")]
use chrono::NaiveDateTime;
use chrono::{DateTime, Local, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::model::Ts;

/// One consecutive block of a charging plan. Periods with `power_limit == 0`
/// (W) are intentional gaps (future TOU pause behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargingPeriod {
    pub start: Ts,
    pub duration_s: u64,
    pub power_limit: u32,
}

impl fmt::Display for ChargingPeriod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.start.to_utc().naive_local().to_string())?;
        f.write_str(" -> ")?;
        f.write_str(
            &(self.start.to_utc() + TimeDelta::seconds(self.duration_s as _))
                .naive_local()
                .to_string(),
        )?;
        f.write_str(" (")?;
        f.write_fmt(format_args!("{:.3} kW", self.power_limit as f32 / 1_000.0))?;
        f.write_char(')')
    }
}

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

impl fmt::Display for PlanKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use PlanKind::*;
        f.write_str(match self {
            Deadline => "Deadline",
            Asap => "Asap",
            Blocking => "Blocking",
            Unlimited => "Unlimited",
        })
    }
}

impl FromStr for PlanKind {
    type Err = ChargingPlanError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Deadline" => PlanKind::Deadline,
            "Asap" => PlanKind::Asap,
            "Blocking" => PlanKind::Blocking,
            "Unlimited" => PlanKind::Unlimited,
            _ => return Err(ChargingPlanError::UnknownPlanKind(s.to_string())),
        })
    }
}

/// The EVSE / CLI charging-plan model: what the server applies to the
/// charging point (OCPP `SetChargingProfile`) and what the web app requests /
/// is notified of (`notification::Notification::ChargingPlan`). Shared by the
/// server, the WASM web front end and the OLED status app.
///
/// The OCPP schedule computation for a plan lives in the server crate
/// (`schedule::ChargingSchedule::try_from_charging_plan`) — it builds the
/// server-local `ChargingSchedule` type.
#[derive(Debug, Default, Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChargingPlan {
    /// Permanent 0 W plan.
    #[default]
    Blocking,
    /// Reach `target_soc` (*fraction* 0–1) by the local time of day
    /// `deadline`.
    Deadline {
        deadline: Ts,
        target_soc: f32,
        power_limit: u32,
    },
    /// Start now, stop at `target_soc` (*fraction* 0–1).
    Asap { target_soc: f32, power_limit: u32 },
    /// No plan in effect (no restriction on charging power).
    Unlimited,
}

impl ChargingPlan {
    #[cfg(feature = "server")]
    pub fn from_db(
        plan_kind: &str,
        deadline: Option<NaiveDateTime>,
        target_soc: Option<f32>,
        power_limit: Option<i64>,
    ) -> Result<Self, ChargingPlanError> {
        use ChargingPlanError::*;

        let kind = PlanKind::from_str(plan_kind)?;

        let get_power_limit = move || -> Result<_, _> {
            let Some(power_limit) = power_limit else {
                return Err(MissingPowerLimit { kind });
            };
            u32::try_from(power_limit).map_err(|_| PowerLimitOutOfRange { power_limit, kind })
        };

        let get_target_soc = move || -> Result<_, _> {
            let target_soc = target_soc.ok_or(MissingTargetSoC { kind })?;
            if !(0.0..=1.0).contains(&target_soc) {
                return Err(TargetSoCOutOfRange { target_soc, kind });
            }

            Ok(target_soc)
        };

        use PlanKind::*;
        let this = match kind {
            Blocking => ChargingPlan::Blocking,
            Deadline => ChargingPlan::Deadline {
                deadline: deadline.ok_or(MissingDeadline { kind })?.and_utc().into(),
                power_limit: get_power_limit()?,
                target_soc: get_target_soc()?,
            },
            Asap => ChargingPlan::Asap {
                power_limit: get_power_limit()?,
                target_soc: get_target_soc()?,
            },
            Unlimited => ChargingPlan::Unlimited,
        };

        Ok(this)
    }

    pub fn is_blocking(&self) -> bool {
        self.kind() == PlanKind::Blocking
    }

    pub fn is_unlimited(&self) -> bool {
        self.kind() == PlanKind::Unlimited
    }

    pub fn kind(&self) -> PlanKind {
        use ChargingPlan::*;
        match self {
            Blocking => PlanKind::Blocking,
            Deadline { .. } => PlanKind::Deadline,
            Asap { .. } => PlanKind::Asap,
            Unlimited => PlanKind::Unlimited,
        }
    }

    pub fn deadline(&self) -> Option<Ts> {
        match self {
            ChargingPlan::Deadline { deadline, .. } => Some(*deadline),
            _ => None,
        }
    }

    pub fn power_limit(&self) -> Option<u32> {
        match self {
            ChargingPlan::Deadline { power_limit, .. } | ChargingPlan::Asap { power_limit, .. } => {
                Some(*power_limit)
            }
            _ => None,
        }
    }

    /// The target SoC (*fraction* 0–1) this plan charges up to, if it has one.
    pub fn target_soc(&self) -> Option<f32> {
        match self {
            Self::Deadline { target_soc, .. } | Self::Asap { target_soc, .. } => Some(*target_soc),
            _ => None,
        }
    }
}

impl fmt::Display for ChargingPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ChargingPlan::*;
        match self {
            Blocking => f.write_str("Blocking")?,
            Unlimited => f.write_str("Unlimited")?,
            Deadline {
                deadline,
                target_soc,
                power_limit,
            } => {
                f.write_str("Deadline ")?;
                deadline.to_utc().with_timezone(&Local).fmt(f)?;
                f.write_str(", target SoC ")?;
                f.write_fmt(format_args!("{:.0}", target_soc * 100.0))?;
                f.write_str("%, power limit ")?;
                f.write_fmt(format_args!("{:.3}", *power_limit as f32 / 100.0))?;
                f.write_str("kWh")?;
            }
            Asap {
                target_soc,
                power_limit,
            } => {
                f.write_str("ASAP target SoC ")?;
                f.write_fmt(format_args!("{:.0}", target_soc * 100.0))?;
                f.write_str("%, power limit ")?;
                f.write_fmt(format_args!("{:.3}", *power_limit as f32 / 100.0))?;
                f.write_str("kWh")?;
            }
        }

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChargingPlanError {
    #[error("Unknonw plan kind {0}")]
    UnknownPlanKind(String),
    #[error("Missing power limit for plan {kind}")]
    MissingPowerLimit { kind: PlanKind },
    #[error("Power limit {power_limit} out of range for plan {kind}")]
    PowerLimitOutOfRange { power_limit: i64, kind: PlanKind },
    #[error("Missing target SoC for plan {kind}")]
    MissingTargetSoC { kind: PlanKind },
    #[error("Target SoC {target_soc} out of range for plan {kind}")]
    TargetSoCOutOfRange { target_soc: f32, kind: PlanKind },
    #[error("Missing deadline for plan {kind}")]
    MissingDeadline { kind: PlanKind },
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
            .map(|p| Ts(p.start.0 + (p.duration_s as i64) * 1_000))
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
// FIXME this should be an enum with fields depending on the PlanKind
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub energy_needed: u32,
    pub soc_delta: f32,
    pub target_soc: f32,
    pub duration: u64,
    pub completes_at: Ts,
    pub feasible: bool,
    /// Average power received by the battery, in W.
    pub avg_battery: u32,
    pub periods: Vec<ChargingPeriod>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// FIXME this should be an enum with fields depending on the PlanKind
pub struct Plan {
    pub id: String,
    pub kind: PlanKind,
    pub start_at: Ts,
    pub target_soc: f32,
    pub deadline: Option<Ts>,
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
            .map(|p| p.start.to_utc() + TimeDelta::seconds(p.duration_s as i64))
            .max()
            .unwrap_or(self.start_at.to_utc())
    }

    /// Power limit in W at `ts`, or 0 when no period covers `ts`.
    pub fn limit_at(&self, ts: DateTime<Utc>) -> u32 {
        self.periods
            .iter()
            .find(|p| {
                let start = p.start.to_utc();
                ts >= start && ts < start + TimeDelta::seconds(p.duration_s as i64)
            })
            .map(|p| p.power_limit)
            .unwrap_or(0)
    }
}

/// Snapshot of the active plan with live progress, as shown on the status tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LivePlan {
    pub plan: Plan,
    pub remaining_energy: u32,
    pub remaining_time: u64,
    pub eta: Ts,
}
