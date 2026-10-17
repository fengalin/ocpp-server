//! Application state shared by the HTTP layer and the simulator task.

use std::sync::Arc;

use chrono::Utc;
use log::info;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::{CommandToChargingPoint, webserver::model};

use super::ingestor::World;
use super::model::{Characteristics, StreamEvent};

pub const BROADCAST_CAPACITY: usize = 512;

#[derive(Clone)]
pub struct AppState {
    pub world: Arc<Mutex<World>>,
    pub front_end_tx: broadcast::Sender<StreamEvent>,
    pub cmd_tx: mpsc::Sender<CommandToChargingPoint>,
}

impl AppState {
    pub fn new(ch: Characteristics, cmd_tx: mpsc::Sender<CommandToChargingPoint>) -> Self {
        let (front_end_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let world = World::new(Utc::now(), &ch);
        Self {
            world: Arc::new(Mutex::new(world)),
            front_end_tx,
            cmd_tx,
        }
    }

    pub async fn send_to_front_end(&self, evt: StreamEvent) -> anyhow::Result<()> {
        match &evt {
            StreamEvent::MeterValues { meter_values, .. } => {
                let mut world = self.world.lock().await;
                world.last_ts = meter_values.ts;
                if let Some(energy) = meter_values.energy {
                    world.energy = energy;
                }
                world.push_meter_values(meter_values.clone());
            }
            StreamEvent::Status { status, session } => {
                let mut world = self.world.lock().await;

                let note = match status {
                    model::Status::Available => {
                        world.session = None;
                        Some("CP available".into())
                    }
                    model::Status::Preparing => {
                        if let Some(session) = session {
                            world.session = Some(session.clone());
                            world.initial_soc = session.initial_soc;
                            world.energy = session.initial_energy;
                        }
                        Some("EV plugged".into())
                    }
                    model::Status::Charging => {
                        if let Some(session) = session {
                            world.session = Some(session.clone());
                            world.initial_soc = session.initial_soc;
                            world.energy = session.initial_energy;
                        }
                        None
                    }
                    model::Status::SuspendedByEVSE => Some("Suspended by EVSE".into()),
                    _ => {
                        world.session = None;
                        None
                    }
                };

                world.push_event(model::EventLogEntry {
                    ts: Utc::now(),
                    status: status.clone(),
                    note,
                });
                world.status = status.clone();
            }
            _ => (),
        }

        // FIXME remove
        info!("pushing to front end: {evt:?}");
        self.front_end_tx.send(evt)?;

        Ok(())
    }
}
