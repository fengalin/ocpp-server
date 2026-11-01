//! WASM front end for the OCPP charge-point web server.
//!
//! Shares the exact wire types with the server via `charge-system-web-model`
//! (and `charge-system-common-model`), so there
//! is no hand-mirrored model. All network payloads are JSON; the canvas
//! plotter (no Chart.js) is hand-rolled in `plot`.

pub mod api;
pub mod dom;
pub mod plot;
pub mod state;
pub mod ui;

use wasm_bindgen::prelude::*;

/// Report a panic to the console AND on-screen, so it is visible even on a
/// phone without dev tools. A panic inside a wasm-bindgen `Closure` handler
/// otherwise traps the whole instance silently (the `init()` promise rejects
/// with no `.catch`), which looks exactly like "the page loaded but nothing
/// works and there is no console output".
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!(
            "PANIC: {}",
            info.payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".into())
        );
        web_sys::console::error_1(&JsValue::from_str(&msg));
        // Best effort: surface it on the plan tab so it is visible on screen.
        if let Some(doc) = web_sys::window().and_then(|w| w.document())
            && let Some(el) = doc.get_element_by_id("plan-msg")
        {
            el.set_text_content(Some(&msg));
            el.set_class_name("msg err");
        }
    }));
}

#[wasm_bindgen(start)]
pub fn main() {
    install_panic_hook();
    ui::init();
}
