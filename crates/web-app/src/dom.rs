//! Small `web-sys` helpers for the DOM patterns the front end uses
//! (`$("id")`, class toggling, text/innerHTML, typed element access).
//!
//! `web-sys` types form a single `JsValue`-backed hierarchy, so a `&T` for any
//! `T: JsCast` is a valid `&Element` / `&HtmlElement` / `&Node` etc. The typed
//! accessors below coerce to the most general type that carries the method we
//! need, so call sites can keep holding whatever concrete type they fetched.

use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Document, Element, HtmlElement, HtmlInputElement, Window};

/// Run `f`, catching a panic so it cannot trap the whole wasm instance.
///
/// A panic inside a wasm-bindgen event `Closure` propagates through the JS
/// call boundary and kills the instance (leaving any `RefCell` borrow wedged),
/// which looks exactly like "the page loaded but nothing works, no console".
/// Catching here turns that into a visible console error instead.
pub fn guard_panic(f: &mut dyn FnMut(&web_sys::Event), e: &web_sys::Event) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(e)));
    if let Err(payload) = result {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        web_sys::console::error_1(&JsValue::from_str(&format!("handler panic: {msg}")));
    }
}

pub fn window() -> Window {
    web_sys::window().expect("no window")
}

pub fn document() -> Document {
    window().document().expect("no document")
}

/// `document.getElementById(id)` — panics when missing, like the old `$(id)`.
pub fn el(id: &str) -> Element {
    document()
        .get_element_by_id(id)
        .unwrap_or_else(|| panic!("missing #{}", id))
}

/// `document.getElementById(id)` without panicking when missing.
pub fn el_option(id: &str) -> Option<Element> {
    document().get_element_by_id(id)
}

pub fn el_html(id: &str) -> HtmlElement {
    el(id)
        .dyn_into::<HtmlElement>()
        .expect("not an html element")
}

/// `document.getElementById(id)` as an `<input>` element.
pub fn input(id: &str) -> HtmlInputElement {
    el_html(id).dyn_into().expect("not an input element")
}

/// Build a panic-guarded event `Closure` and return it as a `js_sys::Function`.
///
/// Use this for every raw `Closure` (timers, `set_on*` assignments, direct
/// `add_event_listener` calls) — exactly like `add_listener` does — so a
/// panic in any handler is logged instead of trapping the wasm instance.
pub fn closure(f: impl FnMut(&web_sys::Event) + 'static) -> js_sys::Function {
    // Coerce to `Box<dyn FnMut>` first: wasm-bindgen's `WasmClosure` is only
    // implemented for trait objects, not concrete closure types.
    let mut f: Box<dyn FnMut(&web_sys::Event)> = Box::new(f);
    // The wrapper closure must itself be `Box<dyn FnMut(&web_sys::Event)>`
    // (a concrete closure does not satisfy `IntoWasmClosure<dyn ...>`).
    let wrapper: Box<dyn FnMut(&web_sys::Event)> =
        Box::new(move |e: &web_sys::Event| guard_panic(&mut f, e));
    let c: wasm_bindgen::closure::Closure<dyn FnMut(&web_sys::Event)> =
        wasm_bindgen::closure::Closure::new(wrapper);
    c.into_js_value().unchecked_into()
}

/// `el.id()` — works for any `JsCast` subtype.
pub fn id_of<T: JsCast>(el: &T) -> String {
    el.unchecked_ref::<Element>().id()
}

/// `el.get_attribute(name)` — works for any `JsCast` subtype.
pub fn attr<T: JsCast>(el: &T, name: &str) -> Option<String> {
    el.unchecked_ref::<Element>().get_attribute(name)
}

/// `el.style.setProperty(prop, value)` — works for any `JsCast` subtype.
pub fn set_style<T: JsCast>(el: &T, prop: &str, value: &str) {
    el.unchecked_ref::<HtmlElement>()
        .style()
        .set_property(prop, value)
        .expect("style setProperty");
}

pub fn set_text(el: &Element, text: &str) {
    el.set_text_content(Some(text));
}

pub fn set_inner_html(el: &Element, html: &str) {
    el.set_inner_html(html);
}

pub fn add_class(el: &Element, cls: &str) {
    el.class_list().add_1(cls).expect("class add");
}

pub fn remove_class(el: &Element, cls: &str) {
    el.class_list().remove_1(cls).expect("class remove");
}

pub fn has_class(el: &Element, cls: &str) -> bool {
    el.class_list().contains(cls)
}

/// Toggle a class, reporting whether it is now present.
pub fn toggle_class(el: &Element, cls: &str) -> bool {
    el.class_list().toggle(cls).expect("class toggle")
}

/// Create a `document.createElement(tag)` element.
pub fn create(tag: &str) -> Element {
    document().create_element(tag).expect("create element")
}

/// `el.add_event_listener(ev, f)` — works for any `JsCast` subtype. The
/// caller keeps the returned `Function` alive (it is the actual listener).
pub fn add_listener<T: JsCast>(
    el: &T,
    ev: &str,
    f: impl FnMut(&web_sys::Event) + 'static,
) -> js_sys::Function {
    let func = closure(f);
    let et: &web_sys::EventTarget = el.unchecked_ref();
    et.add_event_listener_with_callback(ev, &func)
        .expect("addEventListener");
    func
}

pub fn create_html(tag: &str) -> HtmlElement {
    create(tag)
        .dyn_into::<HtmlElement>()
        .expect("create html element")
}

pub fn append(parent: &Element, child: &Element) {
    parent.append_child(child).expect("append child");
}

pub fn query_all(selector: &str) -> web_sys::NodeList {
    document()
        .query_selector_all(selector)
        .expect("querySelectorAll")
}
