//! Web data model shared between the charge-system server and the (WASM) front end.
//!
//! **Only types pertinent to the web app itself are *defined* in this crate**:
//! the web/HTTP+SSE wire contract (`ServerState`, `PlanRequest`, …).
//! Everything shared with the server's other consumers — `Ts`,
//! `MeterValueSelection`, the session / status / event-log types, the
//! scheduling/charging-plan model + computation, and the notification stream
//! itself (the SSE events are `charge_system_common_model::notification::Notification`s) —
//! lives in `charge-system-common-model`, which is used directly from there
//! (no re-export modules in this workspace).
//!
//! Intentionally dependency-light (no OCPP, no tokio, no axum) so the exact
//! same types compile natively for the server and to
//! `wasm32-unknown-unknown` for the front end.

pub mod model;
