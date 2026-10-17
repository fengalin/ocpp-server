//! Pure charging-plan computation. The preview endpoint and plan creation use
//! the exact same code path, so what the user previews is what gets applied.

use chrono::{DateTime, Duration, Utc};

use super::model::{
    Characteristics, ChargingPeriod, LivePlan, Plan, PlanKind, PlanPreview, Session,
    battery_energy_needed,
};

/// Power the battery actually receives at the CP's max grid power
/// (grid power minus static losses), in W.
pub fn max_battery_power(ch: &Characteristics) -> u32 {
    ch.max_power.saturating_sub(ch.static_loss)
}

/// Grid power limit (W) needed to deliver `required_battery_power` (W) to the
/// battery, clamped to the CP maximum.
pub fn clamp_limit(required_battery_power: f64, ch: &Characteristics) -> u32 {
    let limit = (required_battery_power + ch.static_loss as f64).round() as u32;
    limit.min(ch.max_power)
}

/// Compute the preview for a plan.
pub fn compute(
    kind: PlanKind,
    now: DateTime<Utc>,
    cur_soc: f32,
    target_soc: f32,
    deadline: Option<DateTime<Utc>>,
    ch: &Characteristics,
) -> PlanPreview {
    if target_soc <= cur_soc {
        return PlanPreview {
            energy: 0,
            soc_delta: 0.0,
            soc_result: cur_soc,
            duration: 0,
            completes_at: now,
            feasible: true,
            avg_battery: 0,
            periods: vec![],
            note: Some("already at or above target SoC".into()),
        };
    }

    let energy = battery_energy_needed(cur_soc, target_soc, ch);
    let battery_power = max_battery_power(ch);

    match kind {
        PlanKind::Asap => {
            let duration = (energy / battery_power as f64 * 3600.0).round() as u64;
            let completes_at = now + Duration::seconds(duration as i64);
            PlanPreview {
                energy: energy.round() as u64,
                soc_delta: target_soc - cur_soc,
                soc_result: target_soc,
                duration,
                completes_at,
                feasible: true,
                avg_battery: battery_power,
                periods: vec![ChargingPeriod {
                    start_at: now,
                    duration,
                    power_limit: ch.max_power,
                }],
                note: None,
            }
        }
        PlanKind::Deadline => {
            let deadline = deadline.unwrap_or(now + Duration::days(1));
            let available = deadline - now;
            let available_hours = available.num_milliseconds() as f64 / 3_600_000.0;
            let deadline_power = energy / available_hours.max(1.0 / 3600.0);

            if deadline_power <= battery_power as f64 {
                let limit = clamp_limit(deadline_power, ch);
                let duration = available.num_seconds() as u64;
                PlanPreview {
                    energy: energy.round() as u64,
                    soc_delta: target_soc - cur_soc,
                    soc_result: target_soc,
                    duration,
                    completes_at: deadline,
                    feasible: true,
                    avg_battery: deadline_power.round() as u32,
                    periods: vec![ChargingPeriod {
                        start_at: now,
                        duration,
                        power_limit: limit,
                    }],
                    note: None,
                }
            } else {
                // Not reachable in time: warn and finish at the same clock time
                // the next day (mirrors the current CLI behaviour).
                let completes_at = deadline + Duration::days(1);
                let shifted_hours = (completes_at - now).num_milliseconds() as f64 / 3_600_000.0;
                let required_power = energy / shifted_hours;
                let duration = (completes_at - now).num_seconds() as u64;
                PlanPreview {
                    energy: energy.round() as u64,
                    soc_delta: target_soc - cur_soc,
                    soc_result: target_soc,
                    duration,
                    completes_at,
                    feasible: false,
                    avg_battery: required_power.round() as u32,
                    periods: vec![ChargingPeriod {
                        start_at: now,
                        duration,
                        power_limit: clamp_limit(required_power, ch),
                    }],
                    note: Some(
                        "target not reachable by the deadline; the plan finishes at the same \
                         clock time the next day"
                            .into(),
                    ),
                }
            }
        }
    }
}

/// Live progress of an active plan, as shown on the status tab.
pub fn live(
    plan: &Plan,
    session: Option<&Session>,
    now: DateTime<Utc>,
    ch: &Characteristics,
) -> LivePlan {
    let soc = session.map(|s| s.cur_soc).unwrap_or(plan.initial_soc);
    let remaining = battery_energy_needed(soc, plan.target_soc, ch);
    let battery_power = max_battery_power(ch);
    let end = plan.end();
    let remaining_wall = (end - now).num_seconds().max(0) as u64;
    let charging = session.is_some() && plan.limit_at(now) > 0;

    let (remaining_time, eta) = if remaining <= 0.0 {
        (0, now)
    } else if charging {
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
        eta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch() -> Characteristics {
        Characteristics::default()
    }

    #[test]
    fn asap_math() {
        let now = Utc::now();
        let p = compute(PlanKind::Asap, now, 30.0, 80.0, None, &ch());
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
            now + Duration::seconds(expected_duration as i64)
        );
    }

    #[test]
    fn deadline_feasible() {
        let now = Utc::now();
        let d = now + Duration::hours(5);
        let p = compute(PlanKind::Deadline, now, 30.0, 80.0, Some(d), &ch());
        let energy: f64 = 0.5 * 48_100.0; // Wh
        let required_power = energy / 5.0;
        assert!(p.feasible);
        assert_eq!(p.duration, 5 * 3600);
        assert_eq!(p.avg_battery, required_power.round() as u32);
        let limit = p.periods[0].power_limit;
        assert_eq!(limit, ((required_power + 400.0).round() as u32).min(7400));
        assert_eq!(p.completes_at, d);
    }

    #[test]
    fn deadline_infeasible_shifts_to_next_day() {
        let now = Utc::now();
        let d = now + Duration::hours(1);
        let p = compute(PlanKind::Deadline, now, 30.0, 80.0, Some(d), &ch());
        assert!(!p.feasible);
        assert!(p.note.is_some());
        assert_eq!(p.completes_at, d + Duration::days(1));
        assert_eq!(p.duration, 25 * 3600);
    }

    #[test]
    fn already_at_target() {
        let now = Utc::now();
        let p = compute(PlanKind::Asap, now, 90.0, 80.0, None, &ch());
        assert_eq!(p.energy, 0);
        assert_eq!(p.periods.len(), 0);
        assert!(p.feasible);
    }

    #[test]
    fn limit_never_exceeds_cp_max() {
        let now = Utc::now();
        let p = compute(
            PlanKind::Deadline,
            now,
            5.0,
            95.0,
            Some(now + Duration::minutes(30)),
            &ch(),
        );
        assert!(!p.feasible);
        for per in &p.periods {
            assert!(per.power_limit <= 7400);
        }
    }
}
