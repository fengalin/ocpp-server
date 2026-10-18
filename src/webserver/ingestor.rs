use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use log::{debug, warn};
use tokio::sync::broadcast;

use crate::{
    measurements::MeterValueSelection,
    notification::{ChargePointNotification, ChargeState},
};

use super::api::ApiError;
use super::model::{
    self, Characteristics, EventLogEntry, MAX_EVENTS, MAX_METER_VALUES, Plan, PlanKind,
    PlanPreview, PlanState, ServerState, Session, Status, StreamEvent,
};
use super::plan;
use super::state::AppState;

pub async fn into_task(
    state: AppState,
    // FIXME this is a copy of the initial Bms, not current state
    bms: crate::Bms,
    mut notif_rx: broadcast::Receiver<ChargePointNotification>,
) {
    let mut is_prepared = false;
    let mut is_charging = false;
    let mut last_soc = bms.current_soc.inner();

    loop {
        let Ok(notif) = notif_rx.recv().await else {
            warn!("notif chan termination");
            break;
        };

        debug!("notif {notif:?}");
        let evt = match notif {
            ChargePointNotification::MeterValues {
                meter_values: mv,
                soc,
            } => {
                last_soc = soc;

                // FIXME use ChargePointNotification directly
                Some(StreamEvent::Sample {
                    meter_values: mv,
                    soc: last_soc,
                })
            }
            ChargePointNotification::Charge(ChargeState::Available) => {
                is_prepared = false;
                is_charging = false;

                Some(StreamEvent::Status {
                    status: model::Status::Available,
                    session: None,
                })
            }
            ChargePointNotification::Charge(ChargeState::Preparing) => {
                if is_prepared {
                    continue;
                }
                is_prepared = true;
                is_charging = false;

                Some(StreamEvent::Status {
                    status: model::Status::Preparing,
                    session: Some(model::Session {
                        // FIXME
                        id: "1".to_string(),
                        start_ts: Utc::now(),
                        initial_soc: bms.initial_soc.inner(),
                        cur_soc: last_soc,
                        status: model::Status::Preparing,
                        initial_energy: bms.initial_energy.unwrap_or_default() as u64,
                    }),
                })
            }
            ChargePointNotification::Charge(ChargeState::Charging(progress)) => {
                last_soc = Some(progress.soc as f32);

                if !is_charging {
                    is_charging = true;

                    Some(StreamEvent::Status {
                        status: model::Status::Charging,
                        // FIXME do we need to send it again?
                        session: Some(model::Session {
                            id: "1".to_string(), // FIXME
                            start_ts: Utc::now(),
                            initial_soc: bms.initial_soc.inner(),
                            cur_soc: last_soc,
                            status: model::Status::Preparing,
                            initial_energy: bms.initial_energy.unwrap_or_default() as u64,
                        }),
                    })
                } else {
                    None
                }
            }
            ChargePointNotification::Charge(ChargeState::SuspendedEvse(progress)) => {
                last_soc = Some(progress.soc as f32);
                is_charging = false;

                Some(StreamEvent::Status {
                    status: model::Status::SuspendedByEVSE,
                    // FIXME do we need to send it again?
                    session: Some(model::Session {
                        id: "1".to_string(),
                        start_ts: Utc::now(),
                        initial_soc: bms.initial_soc.inner(),
                        cur_soc: last_soc,
                        status: model::Status::Preparing,
                        initial_energy: bms.initial_energy.unwrap_or_default() as u64,
                    }),
                })
            }
            ChargePointNotification::Charge(ChargeState::SuspendedEv) => {
                is_prepared = false;
                is_charging = false;

                Some(StreamEvent::Status {
                    status: model::Status::Finished,
                    session: None,
                })
            }
            _ => continue,
        };

        if let Some(evt) = evt
            && let Err(err) = state.send_to_front_end(evt).await
        {
            debug!("front end notif chan error {err:#}");
        }
    }
}

/// The authoritative fake world.
pub struct World {
    pub ch: Characteristics,
    pub last_ts: DateTime<Utc>,
    pub status: Status,
    pub session: Option<Session>,
    pub plan: Option<Plan>,
    pub initial_soc: Option<f32>,
    pub cur_soc: Option<f32>,
    pub energy: u64,
    pub meter_values: VecDeque<MeterValueSelection>,
    pub events: VecDeque<EventLogEntry>,
}

impl World {
    /// Fresh world for a scenario, with 24 h of seeded history.
    pub fn new(now: DateTime<Utc>, ch: &Characteristics) -> Self {
        Self {
            ch: *ch,
            last_ts: now,
            status: Status::Available,
            session: None,
            plan: None,
            initial_soc: None,
            cur_soc: None,
            energy: 0,
            meter_values: VecDeque::with_capacity(1024),
            events: VecDeque::with_capacity(256),
        }
    }

    pub fn server_state(&self) -> ServerState {
        let active_plan = match &self.plan {
            Some(p) if p.state == PlanState::Active => {
                Some(plan::live(p, self.session.as_ref(), self.last_ts, &self.ch))
            }
            _ => None,
        };
        ServerState {
            ts: self.last_ts,
            characteristics: self.ch,
            status: self.status,
            soc: self.cur_soc,
            session: self.session.clone(),
            active_plan,
        }
    }

    /// Install a computed preview as the active plan.
    pub fn install_plan(
        &mut self,
        kind: PlanKind,
        target_soc: f32,
        deadline: Option<DateTime<Utc>>,
        preview: PlanPreview,
    ) -> Result<Plan, ApiError> {
        // FIXME ideally, we wouldn't return ApiError here
        let now = self.last_ts;
        let p = Plan {
            id: format!("p-{}", now.timestamp()),
            kind,
            start_at: now,
            target_soc,
            deadline,
            initial_soc: self
                .initial_soc
                // FIXME initial SoC must be defined at this point
                // so we should retrieve it from a non Option
                .ok_or_else(|| ApiError::conflict("initial SoC is unknown"))?,
            periods: preview.periods.clone(),
            state: PlanState::Active,
            preview,
            created_ts: now,
        };
        self.plan = Some(p.clone());
        Ok(p)
    }

    /// Stop the active plan; also ends a running session.
    pub fn stop_plan(&mut self, out: &mut Vec<StreamEvent>) -> bool {
        let has_active = self
            .plan
            .as_ref()
            .is_some_and(|p| p.state == PlanState::Active);
        if !has_active {
            return false;
        }
        if let Some(p) = self.plan.as_mut() {
            p.state = PlanState::Cancelled;
        }
        out.push(StreamEvent::Plan);
        true
    }

    // ------------------------------------------------------------------
    // internals
    // ------------------------------------------------------------------

    pub fn push_meter_values(&mut self, mv: MeterValueSelection) {
        self.meter_values.push_back(mv);
        self.meter_values.retain_back(MAX_METER_VALUES);
    }

    pub fn push_event(&mut self, e: EventLogEntry) {
        self.events.push_back(e);
        self.events.retain_back(MAX_EVENTS);
    }
}
