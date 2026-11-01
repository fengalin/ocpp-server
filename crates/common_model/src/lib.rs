//! Common data model for the charge system.
//!
//! **Everything shared amongst multiple crates lives here: types AND logic.**
//! Shared, dependency-light types that are common to the server and to other
//! applications that consume its output — the WASM web front end *and* an
//! external OLED status display. The OLED app is a client of the server's unix
//! socket (see [`notification`]), so the socket wire contract
//! ([`notification::Notification`] + friends) and the meter-value sample
//! ([`meter_value::MeterValueSelection`]) live here. The scheduling /
//! charging-plan data types ([`plan_model`]) and their computation ([`plan`]:
//! `compute`, `live`, `estimate_soc`) are needed by both the server and the
//! web front end, so they live here too — `charge-system-web-model` holds only
//! what is pertinent to the web app itself.
//!
//! No re-export hub: every type is imported from the module that defines it
//! (e.g. `charge_system_common_model::notification::Notification`), so a type
//! has exactly one canonical path.
//!
//! Intentionally OCPP-agnostic at the type level: the OCPP-specific conversions
//! are behind the `ocpp` feature and are only compiled on the server — never in
//! the WASM front end or the OLED app.
//!
//! This crate must stay buildable standalone (its manifest carries its own
//! `version`/`edition`, not workspace-inherited) because external consumers
//! pull it via a git dependency, which cargo builds in isolation.

pub mod bms;
pub mod meter_value;
pub mod model;
pub mod notification;
pub mod plan;
pub mod plan_model;
