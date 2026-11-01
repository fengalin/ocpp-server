//! Charge-point actions (ported from actions.ts) + the styled confirm dialog
//! (shared by the status tab and the actions tab).

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlButtonElement, HtmlElement, KeyboardEvent};

use crate::ui::catchup::console_error;
use crate::ui::render::session_active;
use crate::ui::{ConfirmResolve, on, with_ui, with_ui_ref};
use crate::{api, dom, state};

fn run_action(
    f: impl Future<Output = Result<(), String>> + 'static,
    ok_msg: &str,
    on_change: &Rc<dyn Fn() + 'static>,
) {
    let ok_msg = ok_msg.to_string();
    let on_change = on_change.clone();
    spawn_local(async move {
        match f.await {
            Ok(()) => {
                let el = dom::el("act-msg");
                dom::set_text(&el, &ok_msg);
                el.set_class_name("msg ok");
            }
            Err(e) => {
                let el = dom::el("act-msg");
                dom::set_text(&el, &e);
                el.set_class_name("msg err");
            }
        }
        (on_change)();
    });
}

/// Wire a charge-point action button through the styled confirm dialog and
/// run `f` on confirmation. The button lives for the page, so `f` (one-shot)
/// is stashed in an `Option` and taken on the first *confirmed* click — a
/// cancelled dialog must leave `f` in place for the next click.
fn confirm_action(el: &HtmlElement, msg: &str, f: impl FnOnce() + 'static) {
    let msg = msg.to_string();
    let f = Rc::new(RefCell::new(Some(f)));
    on(el, "click", move |_e| {
        let f = f.clone();
        confirm_show(
            msg.clone(),
            Box::new(move || {
                if let Some(f) = f.borrow_mut().take() {
                    f();
                }
            }),
        );
    });
}

fn el_btn(id: &str) -> HtmlButtonElement {
    dom::el_html(id).dyn_into().expect("button")
}

/// MeterValue sample interval (s) sent by "Set": the dense cadence used for
/// detailed logging (the CP reports ~150 s after its firmware update — see
/// `ocpp.rs`).
const METER_VALUE_INTERVAL_DENSE: u16 = 150;
/// MeterValue sample interval (s) sent by "Reset": the CP's standard cadence
/// (its current factory default — see `ocpp.rs`).
const METER_VALUE_INTERVAL_DEFAULT: u16 = 4126;

pub(crate) fn init_actions(on_change: Rc<dyn Fn() + 'static>) {
    let oc = on_change.clone();
    confirm_action(
        &el_btn("act-stop-session"),
        "Stop the current charging session?",
        move || run_action(api::stop_session(), "Session stopped.", &oc),
    );
    let oc = on_change.clone();
    confirm_action(
        &el_btn("act-stop-plan"),
        "Stop the active plan? A running session is blocked at 0 W until a new plan is applied.",
        move || run_action(api::stop_plan(), "Plan stopped.", &oc),
    );
    let oc = on_change.clone();
    confirm_action(
        &el_btn("act-reboot"),
        "Reboot the charge point?",
        move || run_action(api::reboot_cp(), "Reboot issued.", &oc),
    );
    let oc = on_change.clone();
    on(
        &el_btn("act-set-meter-value-interval"),
        "click",
        move |_e| {
            run_action(
                api::set_meter_value_interval(METER_VALUE_INTERVAL_DENSE),
                "Set MeterValue interval issued.",
                &oc,
            );
        },
    );
    let oc = on_change.clone();
    on(
        &el_btn("act-reset-meter-value-interval"),
        "click",
        move |_e| {
            run_action(
                api::set_meter_value_interval(METER_VALUE_INTERVAL_DEFAULT),
                "Reset MeterValue interval issued.",
                &oc,
            );
        },
    );
}

// ---------------------------------------------------------------------
// Confirmation dialog (shared by the status tab and the actions tab)
// ---------------------------------------------------------------------

/// Open the styled confirm dialog with `msg`; `on_ok` runs exactly once on
/// confirm.
pub(crate) fn confirm_show(msg: String, on_ok: Box<dyn FnOnce()>) {
    // The resolve closure lives for the page, so the one-shot `on_ok` must sit
    // behind a `RefCell` and be taken on first use.
    let on_ok = Rc::new(RefCell::new(Some(on_ok)));
    let resolve: ConfirmResolve = Rc::new(RefCell::new(Box::new(move |ok: bool| {
        if ok && let Some(f) = on_ok.borrow_mut().take() {
            f();
        }
    })));
    let already_open = with_ui(|ui| {
        if ui.confirm_open {
            return true;
        }
        ui.confirm_open = true;
        ui.confirm_resolve = Some(resolve);
        false
    });
    if already_open {
        return;
    }
    dom::set_text(&dom::el("confirm-msg"), &msg);
    dom::remove_class(&dom::el("confirm-overlay"), "hidden");
    let _ = dom::el_html("confirm-cancel").focus();
}

fn confirm_hide(ok: bool) {
    // Take the resolve callback *before* invoking it: the callback runs an
    // action that re-enters the UI (e.g. `render()` ->
    // `ensure_deadline_default()` -> `with_ui(..)`), so a live `UI` borrow
    // across the call would panic and trap the whole wasm instance.
    let resolve = with_ui(|ui| {
        dom::add_class(&dom::el("confirm-overlay"), "hidden");
        ui.confirm_open = false;
        ui.confirm_resolve.take()
    });
    if let Some(r) = resolve {
        (r.borrow_mut())(ok);
    }
}

pub(crate) fn init_confirm() {
    on(&dom::el_html("confirm-cancel"), "click", |_e| {
        confirm_hide(false)
    });
    on(&dom::el_html("confirm-ok"), "click", |_e| {
        confirm_hide(true)
    });
    on(&dom::el_html("confirm-overlay"), "click", |e| {
        // Close only when the backdrop itself is clicked, not the dialog.
        let hit = e
            .target()
            .and_then(|t| {
                let t_node: web_sys::Node = t.unchecked_into();
                let overlay_node: web_sys::Node = dom::el_html("confirm-overlay").unchecked_into();
                t_node.is_same_node(Some(&overlay_node)).then_some(())
            })
            .is_some();
        if hit {
            confirm_hide(false);
        }
    });
    let doc = dom::document();
    on(&doc, "keydown", move |e| {
        let e = e.unchecked_ref::<KeyboardEvent>();
        if e.key() == "Escape" && with_ui_ref(|ui| ui.confirm_open) {
            confirm_hide(false);
        }
    });

    // Status-tab stop-session entry points (badge + button).
    let badge = dom::el_html("status-badge");
    let stop_btn = dom::el_html("stop-session-btn");
    let attach = |el: &web_sys::HtmlElement| {
        on(el, "click", move |_e| {
            let can_stop = state::with_state(|s| {
                s.server
                    .as_ref()
                    .is_some_and(|srv| session_active(&srv.status))
            });
            if with_ui_ref(|ui| ui.confirm_open) || !can_stop {
                return;
            }
            confirm_show(
                "Stop the current charging session?".to_string(),
                Box::new(|| {
                    spawn_local(async {
                        if let Err(e) = api::stop_session().await {
                            console_error("stop session", &e);
                        }
                    });
                }),
            );
        });
    };
    attach(&badge);
    attach(&stop_btn);

    // Status-tab stop-plan entry point (active-plan card; the render shows it
    // only for plans that can be stopped, so no extra gating here).
    let plan_stop = dom::el_html("plan-stop-btn");
    on(&plan_stop, "click", move |_e| {
        if with_ui_ref(|ui| ui.confirm_open) {
            return;
        }
        confirm_show(
            "Stop the active plan? A running session is blocked at 0 W until a new plan is applied.".to_string(),
            Box::new(|| {
                spawn_local(async {
                    if let Err(e) = api::stop_plan().await {
                        console_error("stop plan", &e);
                    }
                });
            }),
        );
    });
}
