//! Live-update protocol: the SSE event handlers and the serial refresh
//! (state → events → metrics window).
//!
//! Steady state is fully event-driven: incoming SSE events are applied
//! locally and rendered — they never trigger a REST fetch. A refresh runs
//! only on boot, SSE (re)open, `resync` (server-side lag), a metrics
//! timeframe change (samples step only) and after user actions.

use charge_system_common_model::{
    bms::Bms,
    meter_value::MeterValueSelection,
    model::{EventLogEntry, Ts},
    notification::ChargeStatus,
    plan_model::{AppliedChargingPlan, ChargingPlan},
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::ui::render::{active_tab, render};
use crate::ui::{now_ms, plan, with_ui};
use crate::{api, plot, state};

/// How much a refresh fetches: everything, or just the sample window (e.g.
/// after a metrics timeframe change).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RefreshScope {
    All,
    MetricsOnly,
}

/// Serial refresh: state → events → samples, each step applied as it lands
/// and failing independently of the others (events are retrieved even if the
/// sample fetch fails). The latest-wins dedup keeps a boot call from
/// doubling with the one triggered by the SSE open.
pub(crate) fn refresh(scope: RefreshScope) {
    let already = with_ui(|ui| {
        if ui.refreshing {
            // A newer trigger arrived while one is in flight: re-run (full
            // scope, the superset) once this one completes.
            ui.refresh_pending = true;
            return true;
        }
        ui.refreshing = true;
        false
    });
    if already {
        return;
    }

    spawn_local(async move {
        if scope != RefreshScope::MetricsOnly {
            // 1. Server state — the source of truth; it overrides any local
            // default, so the status/plan cards render from it right away.
            match api::get_state().await {
                Ok(server) => {
                    state::bump_rx_ts(server.ts.0);
                    state::with_state(|s| s.server = Some(server));
                    render();
                }
                Err(e) => console_error("refresh: state", &e),
            }

            // 2. Event history — independent of the metrics fetch (step 3).
            match api::get_events().await {
                Ok(events) => {
                    state::with_state(|s| {
                        s.events = events.into_iter().rev().collect();
                        if let Some(top) = s.events.first()
                            && let ChargeStatus::Charging(_) | ChargeStatus::SuspendedEvse(_) =
                                top.status
                        {
                            s.update_from_charge_status(top.status);
                        }
                    });
                    plan::countdown_rebaseline_from_state();
                    render();
                }
                Err(e) => console_error("refresh: events", &e),
            }
        }

        // 3. Metric values for the currently selected timeframe window.
        let (from, to) = state::with_state(|s| {
            let anchor = s.server.as_ref().map(|srv| srv.ts.0).unwrap_or_else(now_ms);
            (
                state::to_iso_utc(anchor - s.window),
                state::to_iso_utc(anchor),
            )
        });
        match api::get_metrics(&from, &to).await {
            Ok(metrics) => {
                let last_ts = metrics.last().map(|m| m.ts.0);
                state::with_state(|s| s.metrics = metrics);
                if let Some(ts) = last_ts {
                    state::bump_rx_ts(ts);
                }
                if active_tab() == "metrics" {
                    // All enabled graphs redraw together on the shared window.
                    plot::sync();
                }
            }
            Err(e) => console_error("refresh: metrics", &e),
        }

        let run_again = with_ui(|ui| {
            ui.refreshing = false;
            std::mem::take(&mut ui.refresh_pending)
        });
        if run_again {
            refresh(RefreshScope::All);
        }
    });
}

pub(crate) fn console_error(what: &str, err: &str) {
    web_sys::console::error_1(&JsValue::from_str(&format!("{what}: {err}")));
}

/// SSE `charging_plan` handler: apply the plan now in effect on the EVSE (or
/// its absence, `Unlimited`) to the server state, including the target SoC it
/// sets — then re-seed the target widget and render. Mirrors what the
/// webserver's recording task records into its world.
pub(crate) fn on_charging_plan(applied: AppliedChargingPlan) {
    let known = state::with_state(|s| {
        let Some(ref mut srv) = s.server else {
            return false; // snapshot not in yet (below)
        };
        let target = applied.plan.target_soc();
        srv.charging_plan = match applied.plan {
            ChargingPlan::Unlimited => None,
            _ => Some(applied),
        };
        // The plan's target SoC *is* the EVSE's BMS target: mirror it (plans
        // without a charging target clear any stale value).
        srv.bms.target_soc = target;
        true
    });
    if !known {
        // No state snapshot yet (the page just opened): the event raced the
        // first fetch — re-fetch rather than dropping it (the in-flight
        // response may already have missed the plan).
        refresh(RefreshScope::All);
        return;
    }
    // The plan's target SoC is a server-side value: like a BmsUpdate it may
    // re-seed the target-SoC field (skipped while the user is editing).
    plan::target_sync();
    render();
}

pub(crate) fn on_bms_update(bms: Bms) {
    state::with_state(|s| {
        if let Some(ref mut srv) = s.server {
            srv.bms = bms;
        }
    });
    // A BmsUpdate is the only event that may re-seed the target-SoC field:
    // a local edit persists across renders until the server overrides it.
    plan::target_sync();
    render();
}

pub(crate) fn on_meter_values(mv: MeterValueSelection) {
    let ts = mv.ts.0;
    let energy = mv.energy;
    state::with_state(|s| {
        s.metrics.push(mv);
        // MeterValues with a defined energy is the source of the server BMS
        // copy's `current_energy` (session energy derives from it).
        if let Some(server) = s.server.as_mut()
            && let Some(energy) = energy
        {
            server.bms.update_energy(energy);
        }
        // Keep the visible window anchored to the newest MeterValues so all
        // graphs share the same timeframe. MeterValues are time-ordered, so cut
        // the expired prefix by binary search instead of rescanning the vec.
        if let Some(last) = s.metrics.last() {
            let from = last.ts.0 - s.window;
            let cut = s.metrics.partition_point(|m| m.ts.0 < from);
            if cut > 0 {
                s.metrics.drain(..cut);
            }
        }
    });
    state::bump_rx_ts(ts);
    // Redraw every enabled graph on arrival (even when this update carries
    // no value for one of them) so they stay aligned on the same window.
    if active_tab() == "metrics" {
        plot::sync();
    }
}

/// SSE `status` handler: apply the charge status (and its live SoC and
/// remaining time) to the server state, append the event-log entry it
/// represents on the server side, re-baseline the countdown, then render.
pub(crate) fn on_status(cs: ChargeStatus) {
    // Every status notification is exactly one event-log entry on the
    // server; reconstruct it here (client clock — a refresh replaces the
    // log with the server's authoritative history).
    state::push_event_local(EventLogEntry {
        ts: Ts(now_ms()),
        status: cs,
    });
    state::with_state(|s| {
        s.update_from_charge_status(cs);
    });
    // `render()` re-syncs the countdown from the fresh re-baseline.
    render();
}
