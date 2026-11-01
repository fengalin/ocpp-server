//! Swipe-to-switch-tab (touch + mouse drag) and the desktop wheel equivalent.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::{PointerEvent, WheelEvent};

use super::render::active_tab;
use super::tabs::activate_tab;
use crate::dom;
use crate::ui::{now_ms, on, with_ui, with_ui_ref};

/// A horizontal drag longer than this (css px) is a tab swipe.
const SWIPE_MIN_PX: f64 = 40.0;
/// A drag lasting longer than this (ms) is treated as a slow drag/scroll,
/// not a swipe (so it can't fight the page's vertical scroll).
const SWIPE_MAX_MS: i64 = 350;
/// Horizontal wheel delta (px) that switches a tab.
const WHEEL_SWIPE_PX: i32 = 50;
/// Horizontal drag (css px) beyond which the gesture is claimed as a swipe
/// (and the trailing `click` swallowed).
const SWIPE_CLAIM_PX: f64 = 8.0;
/// A drag must be this much more horizontal than vertical to count.
const SWIPE_DOMINANCE: f64 = 1.5;
/// Accumulated wheel deltas are forgotten after this quiet gap (ms).
const WHEEL_QUIET_MS: i64 = 120;

/// Neighbour of tab `name` in tab-bar order, or `None` at the ends.
fn neighbour(name: &str, dir: i32) -> Option<String> {
    let btns = dom::query_all(".tab");
    let n = btns.length() as i32;
    let mut idx = -1;
    let mut order: Vec<String> = Vec::new();
    for i in 0..n {
        let Some(b) = btns.item(i as u32) else {
            continue;
        };
        let tab =
            dom::attr(&b.unchecked_into::<web_sys::Element>(), "data-tab").unwrap_or_default();
        if tab == name {
            idx = i;
        }
        order.push(tab);
    }
    let next = idx.checked_add(dir)?;
    (next >= 0 && next < n).then(|| order[next as usize].clone())
}

fn swipe_client_xy(e: &web_sys::Event) -> (f64, f64) {
    // PointerEvent has no client_x in web-sys; MouseEvent does (same event).
    let m = e.unchecked_ref::<web_sys::MouseEvent>();
    (m.client_x() as f64, m.client_y() as f64)
}

/// True when the pointer is currently held down on a control that owns the
/// gesture (a range slider, or a SoC number input whose text selection /
/// caret drag must not switch tabs).
fn pointer_on_slider(e: &web_sys::Event) -> bool {
    let Some(target) = e
        .target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return false;
    };
    if dom::attr(&target, "type").as_deref() == Some("range") {
        return true;
    }
    matches!(
        dom::attr(&target, "id").as_deref(),
        Some("target") | Some("init-soc")
    )
}

/// Horizontal swipe (touch) / mouse drag switches between tabs.
///
/// The page keeps native vertical scrolling (`touch-action: pan-y`), and the
/// chart scrub keeps working (it claims the gesture and captures the pointer,
/// so the page never sees the `pointerup`). A swipe is a fast horizontal
/// release: > `SWIPE_MIN_PX` horizontal travel, horizontal-dominant, and
/// shorter than `SWIPE_MAX_MS` (slow drags are left to the browser, which
/// cancels them with `pointercancel` when it starts scrolling).
pub(crate) fn init_swipe() {
    on(&dom::document(), "pointerdown", |e| {
        let p = e.unchecked_ref::<PointerEvent>();
        let (x, y) = swipe_client_xy(e);
        with_ui(|ui| {
            if pointer_on_slider(e) {
                // The gesture is the slider's: remember it so a horizontal drag on
                // the slider never switches tabs (and clear it on release below).
                ui.slider_down = true;
                return;
            }
            ui.swipe.id = Some(p.pointer_id());
            ui.swipe.x = x;
            ui.swipe.y = y;
            ui.swipe.t = now_ms();
            ui.swipe.claimed = false;
        });
    });
    on(&dom::document(), "pointermove", |e| {
        let p = e.unchecked_ref::<PointerEvent>();
        let (x, y) = swipe_client_xy(e);
        with_ui(|ui| {
            let sw = &mut ui.swipe;
            if sw.id != Some(p.pointer_id()) {
                return;
            }
            let dx = x - sw.x;
            let adx = dx.abs();
            let ady = (y - sw.y).abs();
            // Once horizontal motion clearly wins, claim the gesture so the
            // `click` after `pointerup` (mouse) is suppressed below.
            if !sw.claimed && adx > SWIPE_CLAIM_PX && adx > ady * SWIPE_DOMINANCE {
                sw.claimed = true;
            }
        });
    });
    on(&dom::document(), "pointerup", |e| {
        let p = e.unchecked_ref::<PointerEvent>();
        with_ui(|ui| ui.slider_down = false);
        let (x, y) = swipe_client_xy(e);
        // Compute the swipe decision under a *scoped* borrow, so the
        // `was_claimed` block below can re-enter `with_ui` safely (a nested
        // `borrow_mut` would panic and wedge the `RefCell` forever).
        let (was_claimed, next) = with_ui(|ui| {
            let sw = &mut ui.swipe;
            if sw.id != Some(p.pointer_id()) {
                return (false, None);
            }
            let was_claimed = sw.claimed;
            sw.id = None;
            sw.claimed = false;
            let dx = x - sw.x;
            let adx = dx.abs();
            let ady = (y - sw.y).abs();
            let is_touch = p.pointer_type() == "touch";
            // Touch swipes only count when the browser didn't take the gesture
            // (it fires `pointercancel` instead of `pointerup`), and they must
            // be fast: a slow drag is a scroll. Mouse drags never fight scrolling.
            let slow = is_touch && (now_ms() - sw.t) > SWIPE_MAX_MS;
            let next =
                if adx > SWIPE_MIN_PX && adx > ady * SWIPE_DOMINANCE && !slow && !ui.confirm_open {
                    let name = active_tab();
                    let dir = if dx < 0.0 { 1 } else { -1 };
                    neighbour(&name, dir)
                } else {
                    None
                };
            (was_claimed, next)
        });
        if let Some(next) = next {
            activate_tab(&next);
        }
        if was_claimed {
            // Swallow the `click` the browser derives from this drag (mouse
            // only; a touch release never fires `click`). `window.onclick`
            // holds the one-shot handler until it fires, so it does not need
            // to be kept in `UI.alive`.
            let w = dom::window().clone();
            let w2 = w.clone();
            let handler = dom::closure(move |_ce: &web_sys::Event| {
                w2.set_onclick(None);
            });
            w.set_onclick(Some(&handler));
        }
    });
    on(&dom::document(), "pointercancel", |e| {
        let p = e.unchecked_ref::<PointerEvent>();
        with_ui(|ui| {
            ui.slider_down = false;
            if ui.swipe.id == Some(p.pointer_id()) {
                ui.swipe.id = None;
                ui.swipe.claimed = false;
            }
        });
    });
}

/// Desktop equivalent of the swipe: a strong horizontal wheel/trackpad swipe
/// moves to the previous/next tab. Vertical wheel (page scroll) is untouched.
pub(crate) fn init_wheel_tabs() {
    let acc: Rc<RefCell<(i32, i32, i64)>> = Rc::new(RefCell::new((0, 0, 0)));
    let f = move |e: &web_sys::Event| {
        let w = e.unchecked_ref::<WheelEvent>();
        if w.ctrl_key() {
            return; // pinch-zoom
        }
        // A trackpad swipe arrives as a burst of wheel events; accumulate them
        // until a quiet gap, then decide on the totals.
        let (tx, ty, now) = {
            let mut a = acc.borrow_mut();
            let now = now_ms();
            if now - a.2 > WHEEL_QUIET_MS {
                a.0 = 0;
                a.1 = 0;
            }
            a.0 += w.delta_x() as i32;
            a.1 += w.delta_y() as i32;
            a.2 = now;
            (a.0, a.1, now)
        };
        if (tx as i64).abs() <= WHEEL_SWIPE_PX as i64 || (tx as i64).abs() <= (ty as i64).abs() {
            return;
        }
        if with_ui_ref(|ui| ui.confirm_open) {
            return;
        }
        if with_ui_ref(|ui| ui.slider_down) {
            return; // the drag is a slider's, not a tab swipe
        }
        let name = active_tab();
        let dir = if tx < 0 { 1 } else { -1 };
        let Some(next) = neighbour(&name, dir) else {
            return;
        };
        *acc.borrow_mut() = (0, 0, now);
        e.stop_propagation();
        activate_tab(&next);
    };
    let func = dom::closure(f);
    let opts = {
        let o = web_sys::AddEventListenerOptions::new();
        o.set_passive(false);
        o
    };
    dom::window()
        .add_event_listener_with_callback_and_add_event_listener_options("wheel", &func, &opts)
        .expect("addEventListener wheel");
    // The window's listener list keeps the callback alive while attached.
}
