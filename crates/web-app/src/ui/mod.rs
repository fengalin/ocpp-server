//! UI wiring: the global UI handles shared by every UI module.
//!
//! Split into sibling modules: `catchup` (SSE + catch-up protocol),
//! `render` (status / log rendering), `plan` (the plan form + init-SoC
//! widget), `tabs` (tab bar + metric controls), `swipe` (tab-switch
//! gestures) and `actions` (charge-point actions + confirm dialog);
//! `init` wires them together.
//!
//! Ported 1:1 from the old TS front end (`main.ts`, `plan.ts`, `actions.ts`),
//! keeping the same element ids, throttling and confirmation-dialog behaviour.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use charge_system_common_model::notification::ChargeStatus;

use super::{dom, state};

pub mod actions;
pub mod catchup;
pub mod init;
pub mod plan;
pub mod render;
pub mod swipe;
pub mod tabs;

pub use init::init;

/// Confirmation-dialog pending resolution: an `Rc`'d `RefCell` wrapping the
/// callback that resumes the flow once the user answers.
pub type ConfirmResolve = Rc<RefCell<Box<dyn Fn(bool)>>>;

/// Status -> colour (from the old `STATUS_COLORS`).
pub(crate) fn status_color(s: &ChargeStatus) -> &'static str {
    use ChargeStatus::*;
    match s {
        Available => "#7d8590",
        Preparing => "#5aa9e6",
        Charging(_) => "#4cc38a",
        SuspendedEvse(_) => "#e6b45a",
        SuspendedEv => "#9b8ae6",
        StoppedByUser => "#9b8ae6",
        Finishing => "#9b8ae6",
        UnknownSession => "#7d8590",
        Error => "#e05a5a",
    }
}

/// Status -> display name.
pub(crate) fn status_name(s: &ChargeStatus) -> &'static str {
    use ChargeStatus::*;
    match s {
        Available => "Available",
        Preparing => "Preparing",
        Charging(_) => "Charging",
        SuspendedEvse(_) => "Suspended (EVSE)",
        SuspendedEv => "Suspended (EV)",
        StoppedByUser => "Stopped by user",
        Finishing => "Finishing",
        UnknownSession => "Unknown session",
        Error => "Error",
    }
}

pub(crate) fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

// ---------------------------------------------------------------------
// Global UI handles (closures / objects that must outlive their registration)
// ---------------------------------------------------------------------

pub(crate) struct Ui {
    /// Every registered closure + timer id, kept alive for the page lifetime.
    pub alive: Vec<JsValue>,
    /// The SSE `EventSource` (owns the connection).
    pub event_source: Option<JsValue>,
    /// A refresh is in flight (latest-wins dedup).
    pub refreshing: bool,
    /// A newer trigger arrived while a refresh was in flight.
    pub refresh_pending: bool,
    /// Adaptive remaining-time countdown: interval id + its period
    /// (`None` = no timer; the app has no other timers).
    pub countdown_timer: Option<i32>,
    pub countdown_period: Option<i32>,
    /// Plan preview debounce handle.
    pub plan_timer: Option<i32>,
    /// Monotonic id of the latest plan preview/apply request (latest wins).
    pub plan_req_id: i32,
    /// Deadline input already defaulted.
    pub deadline_set: bool,
    /// Power-limit chip selected in the plan form, in W (default: the
    /// highest preset).
    pub power_limit: u32,
    /// Last init-SoC pushed to the server (`None` = never, widget shows
    /// `…` and no value is sent).
    pub init_soc: Option<f32>,
    /// In-flight `set_init_soc` requests. While > 0 (and while the input is
    /// focused), the render must not re-seed the widget from server state,
    /// which still holds the previous value until the commit's echo lands.
    pub init_commits_in_flight: u32,
    /// In-flight plan-apply requests; the apply button re-enables when this
    /// drops to zero (see `do_apply_plan`).
    pub apply_in_flight: u32,
    /// Confirmation-dialog pending resolution, if the dialog is open.
    pub confirm_resolve: Option<ConfirmResolve>,
    pub confirm_open: bool,
    /// Pointer swipe state (horizontal drag on the page switches tabs).
    pub swipe: SwipeState,
    /// Set while a pointer is down on a control that owns the gesture (a
    /// range slider, or the SoC number inputs): tab-switch gestures (swipe +
    /// wheel) must not fight it.
    pub slider_down: bool,
}

/// Pending horizontal pointer drag; a fast enough horizontal release moves to
/// the previous/next tab. `id == None` when no pointer is held.
#[derive(Clone, Copy)]
pub(crate) struct SwipeState {
    pub id: Option<i32>,
    pub x: f64,
    pub y: f64,
    pub t: i64,
    /// Set once the drag leaves the `pan-y` zone (charts only) and starts
    /// moving; from then on the gesture is ours and must not trigger a click.
    pub claimed: bool,
}

pub(crate) const SWIPE_STATE_INIT: SwipeState = SwipeState {
    id: None,
    x: 0.0,
    y: 0.0,
    t: 0,
    claimed: false,
};

/// Default power-limit chip selection in the plan form, in W (the highest
/// preset).
pub(crate) const DEFAULT_POWER_LIMIT: u32 = 7400;

thread_local! {
    /// The global UI state. WASM is single-threaded, so a `thread_local`
    /// needs no `unsafe Sync` wrapper; access it only through the scoped
    /// [`with_ui`] / [`with_ui_ref`] helpers so a guard is never held across
    /// a call that may re-enter (a nested `borrow_mut` would panic and wedge
    /// the `RefCell` for the rest of the page lifetime).
    static UI: RefCell<Ui> = RefCell::new(Ui {
        alive: Vec::new(),
        event_source: None,
        refreshing: false,
        refresh_pending: false,
        countdown_timer: None,
        countdown_period: None,
        plan_timer: None,
        plan_req_id: 0,
        deadline_set: false,
        power_limit: DEFAULT_POWER_LIMIT,
        init_soc: None,
        init_commits_in_flight: 0,
        apply_in_flight: 0,
        confirm_resolve: None,
        confirm_open: false,
        swipe: SWIPE_STATE_INIT,
        slider_down: false,
    });
}

/// Scoped mutable access to the global [`UI`] state.
pub(crate) fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> R {
    UI.with(|ui| f(&mut ui.borrow_mut()))
}

/// Scoped shared access to the global [`UI`] state.
pub(crate) fn with_ui_ref<R>(f: impl FnOnce(&Ui) -> R) -> R {
    UI.with(|ui| f(&ui.borrow()))
}

/// Register `f` on `el` and keep both alive for the page lifetime.
pub(crate) fn on<T: JsCast>(el: &T, ev: &str, f: impl FnMut(&web_sys::Event) + 'static) {
    let func = dom::add_listener(el, ev, f);
    with_ui(|ui| ui.alive.push(func.unchecked_into()));
}

pub(crate) fn now_or_server_ts() -> i64 {
    state::with_state(|s| s.server.as_ref().map(|srv| srv.ts.0).unwrap_or_else(now_ms))
}
