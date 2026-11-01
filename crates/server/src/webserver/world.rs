use std::collections::VecDeque;

use chrono::{DateTime, Utc};

use charge_system_common_model::{
    bms::Bms,
    meter_value::MeterValueSelection,
    model::{EventLogEntry, Ts},
    notification::ChargeStatus,
    plan_model::AppliedChargingPlan,
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
    pub charging_plan: Option<AppliedChargingPlan>,
    pub meter_values: VecDeque<MeterValueSelection>,
    pub events: VecDeque<EventLogEntry>,
}

impl World {
    /// Fresh world; history is recorded as notifications arrive.
    pub fn new(now: DateTime<Utc>, bms: Bms) -> Self {
        Self {
            bms,
            last_ts: now,
            charging_plan: None,
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
            .map(|e| e.status.clone())
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

    // ------------------------------------------------------------------
    // internals
    // ------------------------------------------------------------------

    pub fn push_meter_values(&mut self, mv: MeterValueSelection) {
        self.meter_values.push_back(mv);
        self.meter_values.retain_back(MAX_METER_VALUES);
    }

    pub fn push_event(&mut self, e: EventLogEntry) {
        // Recurring SoC-progress updates arrive as repeated `Charging` entries
        // (typically one per meter value). A charging phase keeps exactly two
        // entries in the log: the first one on the switch to Charging (frozen —
        // it marks when the phase started) and the latest received one (live).
        // While a phase holds only its start entry, a new `Charging` entry
        // appends; once both exist, further updates replace the live one. Every
        // other status always appends: `SuspendedEvse` also carries progress,
        // but a suspension is a one-time transition in practice (at most a
        // handful of entries per episode), so it needs no deduplication.
        let mut trailing_charging = 0;
        for ev in self.events.iter().rev() {
            if matches!(ev.status, ChargeStatus::Charging(_)) {
                trailing_charging += 1;
            } else {
                break;
            }
        }
        if matches!(&e.status, ChargeStatus::Charging(_)) && trailing_charging >= 2 {
            // The phase already has its start entry: only the live (latest)
            // entry is updated. The `back()` above guarantees it exists.
            *self.events.back_mut().expect("live Charging entry") = e;
            return;
        }
        self.events.push_back(e);
        self.events.retain_back(MAX_EVENTS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use charge_system_common_model::{bms, notification::ChargeProgress};

    fn entry(ts: i64, status: ChargeStatus) -> EventLogEntry {
        EventLogEntry { ts: Ts(ts), status }
    }

    fn charging(soc: f32) -> ChargeStatus {
        ChargeStatus::Charging(ChargeProgress {
            soc,
            remaining_time: 100,
        })
    }

    #[test]
    fn charging_progress_updates_collapse_into_first_and_last() {
        let mut w = World::new(Utc::now(), Bms::builder(bms::DEFAULT_CAPACITY).build());
        w.push_event(entry(1, ChargeStatus::Available));
        w.push_event(entry(2, charging(0.2)));
        w.push_event(entry(3, charging(0.4)));
        w.push_event(entry(4, charging(0.6)));

        assert_eq!(w.events.len(), 3);
        // The first entry on the switch to Charging keeps its original ts ...
        let start = w.events.get(1).unwrap();
        assert_eq!(start.ts.0, 2);
        // ... and later updates replace the live (latest) entry only.
        let last = w.events.back().unwrap();
        assert_eq!(last.ts.0, 4);
        assert_eq!(last.status, charging(0.6));
        // `status()` still reports the live (latest) progress.
        assert_eq!(w.status(), charging(0.6));
    }

    #[test]
    fn other_statuses_always_append() {
        let mut w = World::new(Utc::now(), Bms::builder(bms::DEFAULT_CAPACITY).build());
        w.push_event(entry(1, charging(0.2)));
        w.push_event(entry(
            2,
            ChargeStatus::SuspendedEvse(ChargeProgress {
                soc: 0.3,
                remaining_time: 90,
            }),
        ));
        w.push_event(entry(
            3,
            ChargeStatus::SuspendedEvse(ChargeProgress {
                soc: 0.4,
                remaining_time: 80,
            }),
        ));
        // A new Charging episode after a different status appends fresh, then
        // collapses into its own start + live pair.
        w.push_event(entry(4, charging(0.5)));
        w.push_event(entry(5, charging(0.6)));
        w.push_event(entry(6, charging(0.7)));

        assert_eq!(w.events.len(), 5);
        assert_eq!(
            w.events.iter().map(|e| e.ts.0).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 6] // entry 5 replaced the live entry (6)
        );
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
