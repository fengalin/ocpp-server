//! Pure charging-plan computation shared by the server webserver, the web
//! mock and the WASM front end.
//!
//! The preview and the applied plan must always match, so this is the single
//! implementation: the front end computes the preview client-side (it already
//! holds the `Bms` + the current SoC from `/api/state`) and the
//! server recomputes it from its own state when the plan is applied.

use chrono::{DateTime, Duration, Utc};

use crate::bms::{self, Bms};
use crate::model::Ts;
use crate::plan_model::{
    ChargingPeriod, LivePlan, Plan, PlanKind, PlanPreview, battery_energy_needed,
};

/// Grid power limit (W) needed to deliver `required_battery_power` (W) to the
/// battery, clamped to the CP maximum.
pub fn clamp_limit(required_battery_power: f64) -> u32 {
    let limit = (required_battery_power + bms::STATIC_LOSS as f64).round() as u32;
    limit.min(bms::MAX_POWER)
}

/// Estimate the current SoC from the last known initial SoC plus the energy
/// delivered since the session started.
///
/// This is the shared re-estimation rule: the server applies it when the user
/// corrects the initial SoC (mid-session), and the front end applies the same
/// math client-side so the plan preview reacts instantly (the server's answer
/// arrives through the state catch-up). SoC arguments are **fractions 0–1**;
/// energies are Wh. `session_energy` is the session's delivered energy — on
/// the wire that is `last energy sample − session.initial_energy`.
pub fn estimate_soc(initial_soc: f32, session_energy: u64, bms: &Bms) -> f32 {
    let delta = (session_energy as f64 / bms.capacity as f64).min(1.0) as f32;
    (initial_soc + delta).clamp(0.0, 1.0)
}

/// Compute the preview for a plan.
///
/// `limit` is the user-chosen grid power limit in W; it drives both the
/// battery power and the `power_limit` of the resulting periods.
pub fn compute(
    kind: PlanKind,
    now: DateTime<Utc>,
    cur_soc: f32,
    target_soc: f32,
    deadline: Option<DateTime<Utc>>,
    limit: u32,
    bms: &Bms,
) -> PlanPreview {
    if target_soc <= cur_soc {
        return PlanPreview {
            energy: 0,
            soc_delta: 0.0,
            soc_result: cur_soc,
            duration: 0,
            completes_at: Ts::from_utc(now),
            feasible: true,
            avg_battery: 0,
            periods: vec![],
            note: Some("already at or above target SoC".into()),
        };
    }

    let energy = battery_energy_needed(cur_soc, target_soc, bms);
    let limit = limit.min(bms::MAX_POWER);
    if limit <= bms::STATIC_LOSS {
        // The grid limit wouldn't even cover the static loss: no power would
        // reach the battery (and the duration math below divides by zero).
        return PlanPreview {
            energy: energy.round() as u64,
            soc_delta: target_soc - cur_soc,
            soc_result: target_soc,
            duration: 0,
            completes_at: Ts::from_utc(now),
            feasible: false,
            avg_battery: 0,
            periods: vec![],
            note: Some(format!(
                "power limit must exceed the {} W static loss",
                bms::STATIC_LOSS
            )),
        };
    }
    let battery_power = limit - bms::STATIC_LOSS;

    match kind {
        PlanKind::Asap => {
            let duration = (energy / battery_power as f64 * 3600.0).round() as u64;
            let completes_at = now + Duration::seconds(duration as i64);
            PlanPreview {
                energy: energy.round() as u64,
                soc_delta: target_soc - cur_soc,
                soc_result: target_soc,
                duration,
                completes_at: Ts::from_utc(completes_at),
                feasible: true,
                avg_battery: battery_power,
                periods: vec![ChargingPeriod {
                    start_at: Ts::from_utc(now),
                    duration,
                    power_limit: limit,
                }],
                note: None,
            }
        }
        PlanKind::Deadline => {
            // Mirrors `ChargingPlan::Deadline` in server/src/schedule.rs: the
            // duration is derived from the energy to add and the available
            // battery power (charging at the limit, not spread over the whole
            // window); the single period ends exactly at the deadline.
            let deadline = deadline.unwrap_or(now + Duration::days(1));
            let duration = (energy / battery_power as f64 * 3600.0).round() as u64;
            let start = deadline - Duration::seconds(duration as i64);

            let (start, feasible, note) = if start + Duration::minutes(2) <= now {
                // The window would have had to start in the past: shift it to
                // the same clock time the next day (mirrors the `schedule.rs`
                // loop + "can't start today" warning).
                let start = start + Duration::days(1);
                (
                    start,
                    false,
                    Some(
                        "target not reachable by the deadline; charging starts the next day".into(),
                    ),
                )
            } else {
                (start, true, None)
            };

            PlanPreview {
                energy: energy.round() as u64,
                soc_delta: target_soc - cur_soc,
                soc_result: target_soc,
                duration,
                completes_at: Ts::from_utc(start + Duration::seconds(duration as i64)),
                feasible,
                avg_battery: battery_power,
                periods: vec![ChargingPeriod {
                    start_at: Ts::from_utc(start),
                    duration,
                    power_limit: limit,
                }],
                note,
            }
        }
        // Blocking / unlimited have no charging periods.
        PlanKind::Blocking | PlanKind::Unlimited => PlanPreview {
            energy: 0,
            soc_delta: 0.0,
            soc_result: cur_soc,
            duration: 0,
            completes_at: Ts::from_utc(now),
            feasible: true,
            avg_battery: 0,
            periods: vec![],
            note: None,
        },
    }
}

/// Live progress of an active plan, as shown on the status tab.
pub fn live(plan: &Plan, bms: &Bms, is_charging: bool, now: DateTime<Utc>) -> LivePlan {
    let soc = bms.current_soc.absolute().unwrap_or(plan.initial_soc);

    // FIXME we'd need SocProgress here
    let remaining = battery_energy_needed(soc, plan.target_soc, bms);
    let battery_power = bms::MAX_AVAILABLE_POWER;
    let end = plan.end();
    let remaining_wall = (end - now).num_seconds().max(0) as u64;
    let is_charging = is_charging && plan.limit_at(now) > 0;

    let (remaining_time, eta) = if remaining <= 0.0 {
        (0, now)
    } else if is_charging {
        let duration = (remaining / battery_power as f64 * 3600.0).round() as u64;
        (
            duration,
            (now + Duration::seconds(duration as i64)).min(end),
        )
    } else {
        (remaining_wall, end)
    };

    LivePlan {
        plan: plan.clone(),
        remaining_energy: remaining.round() as u64,
        remaining_time,
        eta: Ts::from_utc(eta),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bms() -> Bms {
        Bms::builder(bms::DEFAULT_CAPACITY).build()
    }

    #[test]
    fn asap_math() {
        let now = Utc::now();
        let p = compute(
            PlanKind::Asap,
            now,
            0.30,
            0.80,
            None,
            bms::MAX_POWER,
            &bms(),
        );
        let energy: f64 = 0.5 * 48_100.0; // Wh
        assert_eq!(p.energy, energy.round() as u64);
        let expected_duration = (energy / 7000.0 * 3600.0).round() as u64;
        assert_eq!(p.duration, expected_duration);
        assert_eq!(p.periods.len(), 1);
        assert_eq!(p.periods[0].power_limit, 7400);
        assert!(p.feasible);
        assert_eq!(p.avg_battery, 7000);
        assert_eq!(
            p.completes_at,
            Ts::from_utc(now + Duration::seconds(expected_duration as i64))
        );
        assert!((p.soc_delta - 0.5).abs() < f32::EPSILON);
        assert!((p.soc_result - 0.80).abs() < f32::EPSILON);
    }

    #[test]
    fn deadline_charges_at_limit_before_deadline() {
        let now = Utc::now();
        let d = now + Duration::hours(5);
        let p = compute(
            PlanKind::Deadline,
            now,
            0.30,
            0.80,
            Some(d),
            bms::MAX_POWER,
            &bms(),
        );
        let energy: f64 = 0.5 * 48_100.0; // Wh
        // The duration comes from the energy and the battery power (7000 W),
        // not from the now->deadline window; the period ends exactly at the
        // deadline and charges at the limit.
        let expected_duration = (energy / 7000.0 * 3600.0).round() as u64;
        assert!(p.feasible);
        assert_eq!(p.duration, expected_duration);
        assert!(p.duration < 5 * 3600);
        assert_eq!(p.avg_battery, 7000);
        assert_eq!(p.periods[0].power_limit, 7400);
        assert_eq!(p.completes_at, Ts::from_utc(d));
        assert_eq!(
            p.periods[0].start_at,
            Ts::from_utc(d - Duration::seconds(expected_duration as i64))
        );
    }

    #[test]
    fn deadline_infeasible_shifts_to_next_day() {
        let now = Utc::now();
        let d = now + Duration::hours(1);
        let p = compute(
            PlanKind::Deadline,
            now,
            0.30,
            0.80,
            Some(d),
            bms::MAX_POWER,
            &bms(),
        );
        // 24 050 Wh at 7000 W takes ~206 min: it can't start early enough to
        // finish 1 h from now, so the whole window shifts one day later.
        assert!(!p.feasible);
        assert!(p.note.is_some());
        let expected_duration = (24_050.0 / 7000.0_f64 * 3600.0).round() as u64;
        assert_eq!(p.duration, expected_duration);
        assert!(p.duration > 3600);
        // The window is [d + 1 day - duration, d + 1 day].
        assert_eq!(p.completes_at, Ts::from_utc(d + Duration::days(1)));
        assert_eq!(
            p.periods[0].start_at,
            Ts::from_utc(d + Duration::days(1) - Duration::seconds(expected_duration as i64))
        );
    }

    #[test]
    fn deadline_with_low_limit_shifts_to_next_day() {
        // A low user limit stretches the duration past the now->deadline
        // window, so the plan must start the next day.
        let now = Utc::now();
        let d = now + Duration::hours(4);
        let p = compute(PlanKind::Deadline, now, 0.30, 0.80, Some(d), 2_000, &bms());
        // 24 050 Wh at 1600 W takes ~901 min > the 4 h window.
        assert!(!p.feasible);
        let expected_duration = (24_050.0 / 1_600.0_f64 * 3600.0).round() as u64;
        assert_eq!(p.duration, expected_duration);
        assert!(p.duration > 4 * 3600);
        assert_eq!(p.periods[0].power_limit, 2_000);
        assert_eq!(p.avg_battery, 1_600);
        assert_eq!(p.completes_at, Ts::from_utc(d + Duration::days(1)));
    }

    #[test]
    fn limit_is_clamped_to_cp_max() {
        let now = Utc::now();
        let p = compute(PlanKind::Asap, now, 0.30, 0.80, None, 99_000, &bms());
        assert_eq!(p.periods[0].power_limit, 7400);
        assert_eq!(p.avg_battery, 7000);
    }

    #[test]
    fn already_at_target() {
        let now = Utc::now();
        let p = compute(
            PlanKind::Asap,
            now,
            0.90,
            0.80,
            None,
            bms::MAX_POWER,
            &bms(),
        );
        assert_eq!(p.energy, 0);
        assert_eq!(p.periods.len(), 0);
        assert!(p.feasible);
    }

    #[test]
    fn estimate_soc_scales_with_energy() {
        let ch = bms(); // 48_100 Wh
        // 12_025 Wh = 25 % of the capacity.
        let soc = estimate_soc(0.30, 12_025, &ch);
        assert!((soc - 0.55).abs() < 1e-3);
        // No energy yet: the initial SoC is the current SoC.
        assert!((estimate_soc(0.30, 0, &ch) - 0.30).abs() < f32::EPSILON);
        // Clamped at full.
        assert_eq!(estimate_soc(0.90, 48_100, &ch), 1.0);
    }
}
