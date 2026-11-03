//! Init: wires every UI module together and starts the SSE stream.
//!
//! Boot sequence: restore persisted user selections (tab, metrics timeframe,
//! enabled graphs) → wire the modules → open the SSE stream → serial refresh
//! from the server. Steady state is fully event-driven (SSE events applied
//! locally); the only timer ever running is the adaptive remaining-time
//! countdown, which re-arms itself from server updates.

use std::rc::Rc;

use wasm_bindgen::prelude::*;

use super::actions::{init_actions, init_confirm};
use super::catchup::{
    RefreshScope, console_error, on_bms_update, on_charging_plan, on_meter_values, on_status,
    refresh,
};
use super::plan::init_plan;
use super::swipe::{init_swipe, init_wheel_tabs};
use super::tabs::{activate_tab, init_metrics_controls, init_tabs};
use crate::ui::with_ui;
use crate::{api, plot, state};

/// Entry point (called from `#[wasm_bindgen(start)]`).
pub fn init() {
    // Restore the user's selections from a previous page load before the
    // first fetch, so it runs for the right window and on the right tab.
    state::apply_prefs(state::load_prefs());

    init_tabs();
    activate_tab(&state::with_state(|s| s.tab.clone()));
    init_metrics_controls();
    init_plan(Rc::new(|| refresh(RefreshScope::All)));
    init_actions(Rc::new(|| refresh(RefreshScope::All)));
    init_confirm();
    init_swipe();
    init_wheel_tabs();

    // SSE stream; keep the EventSource alive for the page lifetime. Without
    // EventSource support the app still works off the REST catch-up alone.
    match api::connect_stream(api::StreamHandlers {
        on_bms_update: Box::new(on_bms_update),
        on_meter_values: Box::new(on_meter_values),
        on_status: Box::new(on_status),
        on_charging_plan: Box::new(on_charging_plan),
        // A (re)connected stream: refresh everything.
        on_open: Box::new(|| refresh(RefreshScope::All)),
    }) {
        Ok(es) => with_ui(|ui| ui.event_source = Some(es.unchecked_into())),
        Err(e) => {
            console_error("connect SSE stream", &e);
            state::set_link(state::Link::Down);
            super::render::render_header();
        }
    }

    // Seed from the server (deduped against the SSE-open trigger).
    refresh(RefreshScope::All);

    // Window resize -> redraw charts.
    let resize_cb = plot::watch_resizes();
    with_ui(|ui| ui.alive.push(resize_cb.unchecked_into()));
}
