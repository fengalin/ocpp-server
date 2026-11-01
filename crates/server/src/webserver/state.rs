//! Application state shared by the HTTP layer and the recording task.

use std::sync::Arc;

use chrono::Utc;
use log::{debug, warn};
use tokio::sync::{Mutex, broadcast, mpsc};

use charge_system_common_model::{
    bms::{self, Bms},
    model::{EventLogEntry, Ts},
    notification::{ChargeStatus, Notification},
    plan_model::ChargingPlan,
};

use crate::CommandToChargingPoint;

use super::world::World;

#[derive(Debug, Clone)]
pub struct AppState {
    pub world: Arc<Mutex<World>>,
    // keeping the Sender around so we can subscribe to the channel
    // without keeping around a Receiver which would buffer messages
    // even if there are no consumers for them
    pub notif_tx: broadcast::Sender<Notification>,
    pub cmd_tx: mpsc::Sender<CommandToChargingPoint>,
}

impl AppState {
    pub fn new(
        cmd_tx: mpsc::Sender<CommandToChargingPoint>,
        notif_tx: broadcast::Sender<Notification>,
    ) -> Self {
        let world = World::new(Utc::now(), Bms::builder(bms::DEFAULT_CAPACITY).build());
        Self {
            world: Arc::new(Mutex::new(world)),
            notif_tx,
            cmd_tx,
        }
    }

    /// Consume this state and run as the recording task: record notifications
    /// into `world` as they flow by. This task is the sole writer of the
    /// webserver's world — `/api/state`, `/api/metrics` and `/api/events`
    /// serve what it has recorded, and SSE clients get the live stream on
    /// their own (they subscribe to the shared channel directly).
    pub async fn into_task(self, mut notif_rx: broadcast::Receiver<Notification>) {
        loop {
            match notif_rx.recv().await {
                Ok(notif) => {
                    debug!("notif {notif:?}");
                    self.record(&notif).await;
                }
                // A lag just means a few events were skipped: clients resync from
                // the REST snapshot on demand, so keep recording rather than
                // terminating (a dead recorder would leave `/api/state` stale).
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("notif chan lagged by {n}, continuing");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }

        warn!("notif chan termination");
    }

    /// Record a notification as it flows by, keeping `world` (and therefore
    /// `/api/state`, `/api/metrics`, `/api/events`) up to date. No relay: SSE
    /// clients subscribe to the channel directly.
    pub async fn record(&self, notif: &Notification) {
        match notif {
            Notification::BmsUpdate(bms) => {
                let mut world = self.world.lock().await;
                world.bms = bms.clone();
            }
            Notification::MeterValues(meter_values) => {
                let mut world = self.world.lock().await;
                world.last_ts = meter_values.ts.to_utc();
                // A sample with a defined energy is the source of the world
                // BMS's `current_energy` (session energy derives from it).
                if let Some(energy) = meter_values.energy {
                    world.bms.current_energy = Some(energy);
                }
                world.push_meter_values(meter_values.clone());
            }
            Notification::Status(status) => {
                let mut world = self.world.lock().await;

                if let ChargeStatus::Charging(progress) | ChargeStatus::SuspendedEvse(progress) =
                    &status
                {
                    // A charge-progress update carries the current SoC; apply
                    // it to the world BMS so `/api/state` and the frontend
                    // stay current between meter values (which own
                    // `current_energy`).
                    world.bms.current_soc = bms::SoC::Absolute(progress.soc);
                }

                world.push_event(EventLogEntry {
                    ts: Ts::from_utc(Utc::now()),
                    status: status.clone(),
                });
            }
            Notification::Connection(_) => (),
            Notification::ChargingPlan(applied) => {
                let mut world = self.world.lock().await;
                // `Unlimited` means no plan is in effect (the charging point's
                // charging is unrestricted); every other variant — including
                // the default post-session `Blocking` — is the plan now active
                // on the EVSE.
                world.charging_plan = match applied.plan {
                    ChargingPlan::Unlimited => None,
                    _ => Some(applied.clone()),
                };
                // The plan's target SoC *is* the EVSE's BMS target: mirror it
                // onto the world BMS (plans without a charging target clear
                // any stale value — the EVSE no longer sends a separate
                // BmsUpdate on the plan path).
                world.bms.target_soc = applied.plan.target_soc();
            }
        }
    }
}
