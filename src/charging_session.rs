use chrono::{DateTime, Local, Utc};
use ocpp_rs::v16::{
    call::{StartTransaction, StopTransaction},
    enums::Reason,
};

const BATTERY_CAPACITY: f32 = 48_100.0;

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ChargingSessionSnapshot {
    pub timestamp: DateTime<Local>,
    pub meter: u64,
    pub soc: Option<f32>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ChargingSession {
    pub transaction_id: i32,
    pub snapshots: Vec<ChargingSessionSnapshot>,
    pub stop_reason: Option<Reason>,
}

impl ChargingSession {
    pub fn new(
        transaction_id: i32,
        start_transaction_call: &StartTransaction,
        soc: Option<f32>,
    ) -> Self {
        ChargingSession {
            transaction_id,
            snapshots: vec![ChargingSessionSnapshot {
                timestamp: start_transaction_call
                    .timestamp
                    .inner()
                    .with_timezone(&Local),
                meter: start_transaction_call.meter_start,
                soc,
            }],
            stop_reason: None,
        }
    }

    pub fn start_timestamp(&mut self) -> DateTime<Local> {
        self.snapshots
            .first()
            .expect("added by Self::new")
            .timestamp
    }

    pub fn add_snapshot(&mut self, timestamp: DateTime<Utc>, meter: u64) {
        let mut snapshot = ChargingSessionSnapshot {
            timestamp: timestamp.with_timezone(&Local),
            meter,
            soc: None,
        };
        let first_snapshot = self.snapshots.first().expect("added by new");
        if let Some(start_soc) = first_snapshot.soc
            && let Some(meter_delta) = meter.checked_sub(first_snapshot.meter)
        {
            snapshot.soc = Some(meter_delta as f32 / BATTERY_CAPACITY + start_soc);
        }
        self.snapshots.push(snapshot);
    }

    pub fn stop(&mut self, stop_transaction_call: &StopTransaction) {
        self.add_snapshot(
            stop_transaction_call.timestamp.inner(),
            stop_transaction_call.meter_stop,
        );
        self.stop_reason = stop_transaction_call.reason;
    }
}
