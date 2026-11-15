use std::collections::VecDeque;

use chrono::{DateTime, Utc};

use charge_system_common_model::{
    bms::Bms,
    meter_value::MeterValueSelection,
    model::{EventLogEntry, Ts},
    notification::ChargeStatus,
    plan_model::{AppliedChargingPlan, ChargingPlan},
};

use charge_system_web_model::model::{MAX_EVENTS, MAX_METER_VALUES, ServerState};

/// The webserver's world: everything the REST endpoints serve, recorded from
/// the notification stream by the recording task.
#[derive(Debug)]
pub struct World {
    pub bms: Bms,
    pub last_ts: DateTime<Utc>,
    /// The plan currently in effect on the EVSE, with its computed schedule
    /// (recorded from `Notification::ChargingPlan`; `None` when none).
    pub charging_plan: AppliedChargingPlan,
    pub meter_values: VecDeque<MeterValueSelection>,
    pub events: VecDeque<EventLogEntry>,
}

impl World {
    /// Fresh world; history is recorded as notifications arrive.
    pub fn new(now: DateTime<Utc>, bms: Bms) -> Self {
        Self {
            bms,
            last_ts: now,
            charging_plan: AppliedChargingPlan {
                plan: ChargingPlan::Unlimited,
                periods: vec![],
            },
            meter_values: VecDeque::with_capacity(MAX_METER_VALUES),
            events: VecDeque::with_capacity(MAX_EVENTS),
        }
    }

    /// The current charge status: the newest event in the log (the log is
    /// append-only and every status change lands there, so its last entry is
    /// the live status). `Available` until the first notification arrives.
    pub fn status(&self) -> ChargeStatus {
        self.events
            .back()
            .map(|e| e.status)
            .unwrap_or(ChargeStatus::Available)
    }

    pub fn server_state(&self) -> ServerState {
        ServerState {
            bms: self.bms.clone(),
            ts: Ts::from_utc(self.last_ts),
            status: self.status(),
            charging_plan: self.charging_plan.clone(),
        }
    }

    pub fn push_meter_values(&mut self, mv: MeterValueSelection) {
        self.meter_values.push_back(mv);
        self.meter_values.retain_back(MAX_METER_VALUES);
    }

    pub fn push_event(&mut self, e: EventLogEntry) {
        self.events.push_back(e);
        self.events.retain_back(MAX_EVENTS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use charge_system_common_model::bms;

    fn entry(ts: i64, status: ChargeStatus) -> EventLogEntry {
        EventLogEntry { ts: Ts(ts), status }
    }

    #[test]
    fn event_log_is_capped() {
        let mut w = World::new(Utc::now(), Bms::builder(bms::DEFAULT_CAPACITY).build());
        for i in 0..MAX_EVENTS + 10 {
            // Alternate statuses so nothing collapses.
            let status = if i % 2 == 0 {
                ChargeStatus::Available
            } else {
                ChargeStatus::Preparing
            };
            w.push_event(entry(i as i64, status));
        }
        assert_eq!(w.events.len(), MAX_EVENTS);
        assert_eq!(
            w.events.front().unwrap().ts.0,
            10 // the oldest entry survives
        );
    }
}
