use std::{
    collections::BTreeMap,
    fmt::{self, Write},
};

use charge_system_common_model::{
    bms::{self, Bms, BmsError},
    model::Ts,
    plan_model::{AppliedChargingPlan, ChargingPeriod, ChargingPlan},
};
use chrono::{DateTime, Duration, Local, TimeDelta, Utc};
use ocpp_rs::v16::{call, data_types as ocpp_v16, enums::*};
use tracing::{debug, info, warn};

const DEFAULT_NB_OF_PHASES: i32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ChargingScheduleError {
    #[error("end {end} is earlier than start {start}")]
    EndEarlierThanStart { start: String, end: String },
    #[error("Couldn't compute energy to add: {0}")]
    TargetSoCToEnergy(#[from] BmsError),
}

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct RemainingDurationEnergy {
    pub duration: Duration,
    pub energy: u32,
}
impl RemainingDurationEnergy {
    fn zero() -> Self {
        Default::default()
    }

    pub fn is_zero(&self) -> bool {
        *self == RemainingDurationEnergy::zero()
    }

    fn add(&mut self, delta: TimeDelta, active_power: u32) {
        assert!(delta.num_seconds() >= 0);
        self.duration += delta;
        self.energy += active_power * delta.num_seconds() as u32 / 60 / 60;
    }
}

impl fmt::Display for RemainingDurationEnergy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return f.write_char('—');
        }

        let total_secs = self.duration.num_seconds();
        let total_mins = total_secs / 60;
        let hours = total_mins / 60;
        let mins = total_mins % 60;
        let secs = total_secs % 60;
        if hours > 0 {
            f.write_fmt(format_args!("+{hours}:{mins:02}:{secs:02}",))?;
        } else {
            f.write_fmt(format_args!("+{mins:02}:{secs:02}",))?;
        }

        f.write_fmt(format_args!(", {:.3} kWh", self.energy as f32 / 1_000.0))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChargingSchedule {
    pub id: i32,
    pub set_time: DateTime<Utc>,
    pub plan: ChargingPlan,
    pub state: ChargingScheduleState,
    pub periods: BTreeMap<Ts, ChargingPeriod>,
}

impl ChargingSchedule {
    pub fn new_blocking_active() -> Self {
        Self::default()
    }

    pub fn new_unlimited_active() -> Self {
        ChargingSchedule {
            plan: ChargingPlan::Unlimited,
            state: ChargingScheduleState::Active,
            ..Default::default()
        }
    }

    /// Compute the OCPP charging schedule for `plan` from the BMS (the energy to
    /// add, the duration at the net power limit, and which day to start on).
    pub fn try_from_charging_plan(
        plan: ChargingPlan,
        bms: &Bms,
    ) -> Result<ChargingSchedule, ChargingScheduleError> {
        use ChargingPlan::*;
        match &plan {
            Blocking => Ok(ChargingSchedule::new_blocking_active()),
            Deadline {
                deadline,
                target_soc,
                power_limit,
            } => {
                let energy_to_add = bms.target_soc_to_energy(*target_soc)?;

                let available_power = power_limit.saturating_sub(bms::STATIC_LOSS);
                // FIXME would also need a ponderation in case of variations due to DPM
                let duration_s = energy_to_add as u64 * 60 * 60 / available_power as u64;
                let energy_needed = duration_s * *power_limit as u64 / 60 / 60;
                // FIXME add extra duration as we get closer to 100% SoC

                let now = Utc::now();
                let today = now.date_naive();
                let delta = TimeDelta::seconds(duration_s as _);
                let mut start = deadline.to_utc() - delta;
                info!(
                    cur_soc = %bms.current_soc(),
                    target_soc = %bms.target_soc_str(),
                    energy_to_add = %(energy_to_add as f32 / 1_000.0),
                    energy_needed = %(energy_needed as f32 / 1_000.0),
                    loss = %((1.0 - (energy_to_add as f32 / energy_needed as f32)) * 100.0),
                    duration_mn = %delta.num_minutes(),
                    "preparing schedule",
                );

                loop {
                    if start + TimeDelta::minutes(2) > now {
                        // FIXME added a safety margin, needs more thinking + const / conf
                        break;
                    }
                    // charging would have needed to start earlier => select next day
                    start += TimeDelta::days(1);
                }
                if start.date_naive() != today {
                    warn!("charging schedule can't start today");
                }

                let start_local = start.with_timezone(&Local).naive_local();
                info!(
                    start_local = %start_local,
                    end_local = %(start_local + delta),
                    target_soc = %(*target_soc * 100.0),
                    "deadline charging schedule",
                );

                // FIXME add a charging plan to span off teak periods

                let start = start.into();

                Ok(ChargingSchedule {
                    periods: [(
                        start,
                        ChargingPeriod {
                            start,
                            duration_s,
                            power_limit: *power_limit,
                        },
                    )]
                    .into(),
                    plan,
                    ..Default::default()
                })
            }
            Asap {
                target_soc,
                power_limit,
            } => {
                let energy_to_add = bms.target_soc_to_energy(*target_soc)?;

                let available_power = power_limit.saturating_sub(bms::STATIC_LOSS);
                let duration_s = energy_to_add as u64 * 60 * 60 / available_power as u64;

                let now = Utc::now();
                let delta = TimeDelta::seconds(duration_s as _);
                let now_local = now.naive_local();
                info!(
                    start_local = %now_local,
                    end_local = %(now_local + delta),
                    target_soc = %(*target_soc * 100.0),
                    "asap charging schedule",
                );

                let start = now.into();

                Ok(ChargingSchedule {
                    periods: [(
                        start,
                        ChargingPeriod {
                            start,
                            duration_s,
                            power_limit: *power_limit,
                        },
                    )]
                    .into(),
                    plan,
                    ..Default::default()
                })
            }
            Unlimited => Ok(ChargingSchedule {
                plan: ChargingPlan::Unlimited,
                ..Default::default()
            }),
        }
    }

    pub fn refresh(&mut self, bms: &Bms) -> Result<(), ChargingScheduleError> {
        use ChargingPlan::*;
        match self.plan {
            Blocking | Unlimited => self.reset_set_time(),
            Deadline { .. } | Asap { .. } => {
                *self = ChargingSchedule::try_from_charging_plan(self.plan, bms)?;
            }
        }

        Ok(())
    }

    pub fn get_applied_plan(&self) -> AppliedChargingPlan {
        AppliedChargingPlan {
            plan: self.plan,
            periods: self.periods().cloned().collect(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    pub fn add_period(mut self, period: ChargingPeriod) -> Self {
        self.periods.insert(period.start, period);
        self
    }

    pub fn periods(&self) -> impl Iterator<Item = &ChargingPeriod> {
        self.periods.values()
    }

    pub fn is_empty(&self) -> bool {
        self.periods.is_empty()
    }

    pub fn reset_set_time(&mut self) {
        self.set_time = Utc::now();
        self.state = ChargingScheduleState::Active;
    }

    // FIXME energy that will be added also depends on the SoC and DPM profile
    pub fn remaining(
        &self,
        from_instant: DateTime<Utc>,
        constant_power_loss: u32,
    ) -> RemainingDurationEnergy {
        let mut rem = RemainingDurationEnergy::default();
        if !self.is_active() {
            return rem;
        }
        for period in self.periods.values() {
            let period_end = period.start.to_utc() + TimeDelta::seconds(period.duration_s as _);

            if from_instant >= period_end {
                continue;
            }
            if from_instant > period.start.to_utc() && from_instant < period_end {
                rem.add(
                    period_end - from_instant,
                    period.power_limit - constant_power_loss,
                );
                continue;
            }
            rem.add(
                TimeDelta::seconds(period.duration_s as _),
                period.power_limit - constant_power_loss,
            );
        }

        rem
    }
}

impl Default for ChargingSchedule {
    fn default() -> Self {
        ChargingSchedule {
            id: 0,
            set_time: Utc::now(),
            plan: Default::default(),
            state: ChargingScheduleState::Active,
            periods: Default::default(),
        }
    }
}

impl fmt::Display for ChargingSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("id ")?;
        self.id.fmt(f)?;
        f.write_str(", state ")?;
        self.state.fmt(f)?;
        f.write_str(", plan: ")?;
        self.plan.fmt(f)?;
        f.write_str(", set time ")?;
        self.set_time.with_timezone(&Local).fmt(f)?;

        if self.periods.is_empty() {
            f.write_str("\npermanent 0 W (blocking)")?;
            return Ok(());
        }
        for period in self.periods.values() {
            f.write_fmt(format_args!("\n* {period}"))?
        }
        Ok(())
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub enum ChargingScheduleState {
    #[default]
    Active,
    Inactive,
}
impl ChargingScheduleState {
    pub fn is_active(self) -> bool {
        matches!(self, ChargingScheduleState::Active)
    }
    pub fn as_i32(self) -> i32 {
        if !self.is_active() {
            return 0;
        }
        1
    }
}

impl From<i32> for ChargingScheduleState {
    fn from(value: i32) -> Self {
        if value == 0 {
            return ChargingScheduleState::Inactive;
        }

        ChargingScheduleState::Active
    }
}

impl fmt::Display for ChargingScheduleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ChargingScheduleState::*;
        f.write_str(match self {
            Active => "Active",
            Inactive => "Inactive",
        })
    }
}

#[derive(Debug)]
pub struct SetChargingProfile;
impl SetChargingProfile {
    pub fn builder(schedule: ChargingSchedule) -> SetChargingProfileBuilder {
        debug!(
            "SetCharingProfile start {}",
            schedule.set_time.with_timezone(&Local)
        );
        SetChargingProfileBuilder {
            start_schedule: schedule.set_time,
            periods: schedule.periods,
        }
    }
}

#[derive(Debug)]
pub struct SetChargingProfileBuilder {
    start_schedule: DateTime<Utc>,
    periods: BTreeMap<Ts, ChargingPeriod>,
}

impl SetChargingProfileBuilder {
    // only intended at making tests predictable
    #[cfg(test)]
    fn set_schedule_instant(mut self, schedule_start: DateTime<Utc>) -> Self {
        debug!(local_time = %schedule_start.naive_local(), "Schedule start");
        self.start_schedule = schedule_start;
        self
    }

    pub fn build_charging_schedule(self) -> Vec<ocpp_v16::ChargingSchedulePeriod> {
        let start_schedule = self.start_schedule.into();
        let mut charging_schedule = vec![];
        let mut last_end = None;
        for period in self.periods.values() {
            let start = period.start;
            let end = period.start + TimeDelta::seconds(period.duration_s as _);

            if last_end.is_none() {
                // first period
                if start >= start_schedule {
                    // first period starts in the future
                    last_end = Some(start_schedule);
                    // proceed as a regular period
                } else {
                    // first period starts in the past
                    if end > start_schedule {
                        // first period ends in the future
                        debug!(
                            ?period,
                            start_local = %self.start_schedule.naive_local(),
                            "Adding truncated first schedule period",
                        );
                        charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                            start_period: 0,
                            limit: period.power_limit as _,
                            number_phases: Some(DEFAULT_NB_OF_PHASES),
                        });
                        last_end = Some(end);
                    } else {
                        debug!(
                            ?period,
                            "Skipping first schedule period entirely in the past"
                        );
                    }

                    continue;
                }
            }
            // else not first period

            let Some(ref mut last_end) = last_end else {
                unreachable!("checked / assigned above");
            };
            if start >= *last_end {
                if start > *last_end {
                    // add a gap
                    let gap = *last_end - start_schedule;
                    debug!(
                        seconds = (start - *last_end).num_seconds(),
                        "Adding schedule gap"
                    );
                    charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                        start_period: gap.num_seconds() as i32,
                        limit: 0.0,
                        number_phases: Some(DEFAULT_NB_OF_PHASES),
                    })
                }

                let start_period = start - start_schedule;
                debug!(?period, "Adding schedule period");
                charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                    start_period: start_period.num_seconds() as i32,
                    limit: period.power_limit as _,
                    number_phases: Some(DEFAULT_NB_OF_PHASES),
                });
            } else if end > *last_end {
                // this period is set to start before last period ends
                // and it ends after last period's end
                debug!(
                    ?period,
                    start_local = %last_end.to_utc().naive_local(),
                    "Adding truncated schedule period",
                );
                let last_end_period = *last_end - start_schedule;
                charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                    start_period: last_end_period.num_seconds() as i32,
                    limit: period.power_limit as _,
                    number_phases: Some(DEFAULT_NB_OF_PHASES),
                });
            } else {
                debug!(
                    ?period,
                    "Skipping schedule period, overlapping previous period"
                );
            }

            if end > *last_end {
                *last_end = end;
            }
        }

        let last_end = last_end.unwrap_or(start_schedule);

        let last_end_start = last_end - start_schedule;
        if last_end_start.num_seconds() > 0 || charging_schedule.is_empty() {
            // add a zero limit to terminate the schedule
            // or as a safety measure if the configuration would result in an empty schedule
            charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                start_period: last_end_start.num_seconds() as i32,
                limit: 0.0,
                number_phases: Some(DEFAULT_NB_OF_PHASES),
            });
        }

        debug!("resulting ChargingSchedule\n{charging_schedule:#?}");
        charging_schedule
    }

    pub fn build(self) -> call::SetChargingProfile {
        call::SetChargingProfile {
            connector_id: 0,
            cs_charging_profiles: ocpp_v16::ChargingProfile {
                charging_profile_id: 0, // FIXME keep track of this
                transaction_id: None,
                stack_level: 0, // highest gets highest priority
                charging_profile_purpose: ChargingProfilePurposeType::ChargePointMaxProfile,
                // FIXME unsure whether this is considered by the charging point
                charging_profile_kind: ChargingProfileKindType::Absolute,
                // FIXME only weekly supported?
                recurrency_kind: None,
                valid_from: None,
                valid_to: None,
                charging_schedule: ocpp_v16::ChargingSchedule {
                    duration: None,
                    // FIXME seems to be ignored by the charging point
                    // need to clarify (for absolute profile kind):
                    // * does the schedule starts running relatively to submission?
                    // * does the schedule starts running when the transaction starts?
                    // the later would also explain the behaviour observed
                    // when requesting the schedule
                    start_schedule: Some(ocpp_v16::DateTimeWrapper::new(self.start_schedule)),
                    charging_rate_unit: ChargingRateUnitType::W,
                    charging_schedule_period: self.build_charging_schedule(),
                    min_charging_rate: None,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use charge_system_common_model::bms::MAX_POWER;
    use chrono::NaiveTime;

    use super::*;

    #[test]
    pub fn disjointed_in_the_future() {
        crate::tests::init();

        let today = Utc::now().date_naive();
        let set_schedule_instant = today.and_time(NaiveTime::from_hms_opt(12, 00, 00).unwrap());

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = set_schedule_instant + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule_today = SetChargingProfile::builder(
            ChargingSchedule::new_blocking_active()
                .add_period(ChargingPeriod {
                    start: period1_start.and_utc().into(),
                    duration_s: period1_duration.num_seconds() as _,
                    power_limit: period1_limit,
                })
                .add_period(ChargingPeriod {
                    start: period2_start.and_utc().into(),
                    duration_s: period2_duration.num_seconds() as _,
                    power_limit: MAX_POWER,
                }),
        )
        .set_schedule_instant(set_schedule_instant.and_utc())
        .build_charging_schedule();

        assert_eq!(
            charging_schedule_today,
            vec![
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: 0,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: period1_start_delta.num_seconds() as _,
                    limit: period1_limit as _,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: (period1_start_delta + period1_duration).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: period2_start_delta.num_seconds() as _,
                    limit: bms::MAX_POWER as _,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: (period2_start_delta + period2_duration).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
            ],
        );
    }

    #[test]
    pub fn disjointed_first_starting_in_the_past() {
        crate::tests::init();

        let today = Utc::now().date_naive();
        let set_schedule_instant = today.and_time(NaiveTime::from_hms_opt(12, 00, 00).unwrap());

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant - period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = set_schedule_instant + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = SetChargingProfile::builder(
            ChargingSchedule::new_blocking_active()
                .add_period(ChargingPeriod {
                    start: period1_start.and_utc().into(),
                    duration_s: period1_duration.num_seconds() as _,
                    power_limit: period1_limit,
                })
                .add_period(ChargingPeriod {
                    start: period2_start.and_utc().into(),
                    duration_s: period2_duration.num_seconds() as _,
                    power_limit: bms::MAX_POWER,
                }),
        )
        .set_schedule_instant(set_schedule_instant.and_utc())
        .build_charging_schedule();

        assert_eq!(
            charging_schedule,
            vec![
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: 0,
                    limit: period1_limit as _,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: (period1_duration - period1_start_delta).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: period2_start_delta.num_seconds() as _,
                    limit: bms::MAX_POWER as _,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: (period2_start_delta + period2_duration).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
            ],
        );
    }

    #[test]
    pub fn disjointed_first_entirely_in_the_past() {
        crate::tests::init();

        let today = Utc::now().date_naive();
        let set_schedule_instant = today.and_time(NaiveTime::from_hms_opt(12, 00, 00).unwrap());

        let period1_start_delta = TimeDelta::hours(3);
        let period1_start = set_schedule_instant - period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = set_schedule_instant + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = SetChargingProfile::builder(
            ChargingSchedule::new_blocking_active()
                .add_period(ChargingPeriod {
                    start: period1_start.and_utc().into(),
                    duration_s: period1_duration.num_seconds() as _,
                    power_limit: period1_limit,
                })
                .add_period(ChargingPeriod {
                    start: period2_start.and_utc().into(),
                    duration_s: period2_duration.num_seconds() as _,
                    power_limit: MAX_POWER,
                }),
        )
        .set_schedule_instant(set_schedule_instant.and_utc())
        .build_charging_schedule();

        assert_eq!(
            charging_schedule,
            vec![
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: 0,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: period2_start_delta.num_seconds() as _,
                    limit: bms::MAX_POWER as _,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: (period2_start_delta + period2_duration).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
            ],
        );
    }

    #[test]
    pub fn second_partially_overlapping_first() {
        crate::tests::init();

        let today = Utc::now().date_naive();
        let set_schedule_instant = today.and_time(NaiveTime::from_hms_opt(12, 00, 00).unwrap());

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(2);
        let period2_start = set_schedule_instant + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = SetChargingProfile::builder(
            ChargingSchedule::new_blocking_active()
                .add_period(ChargingPeriod {
                    start: period1_start.and_utc().into(),
                    duration_s: period1_duration.num_seconds() as _,
                    power_limit: period1_limit,
                })
                .add_period(ChargingPeriod {
                    start: period2_start.and_utc().into(),
                    duration_s: period2_duration.num_seconds() as _,
                    power_limit: MAX_POWER,
                }),
        )
        .set_schedule_instant(set_schedule_instant.and_utc())
        .build_charging_schedule();

        assert_eq!(
            charging_schedule,
            vec![
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: 0,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: period1_start_delta.num_seconds() as _,
                    limit: period1_limit as _,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: (period1_start_delta + period1_duration).num_seconds() as _,
                    limit: bms::MAX_POWER as _,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: (period2_start_delta + period2_duration).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
            ],
        );
    }

    #[test]
    pub fn second_overlapping_first() {
        crate::tests::init();

        let today = Utc::now().date_naive();
        let set_schedule_instant = today.and_time(NaiveTime::from_hms_opt(12, 00, 00).unwrap());

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(2);
        let period2_start = set_schedule_instant + period2_start_delta;
        let period2_duration = TimeDelta::minutes(30);

        let charging_schedule = SetChargingProfile::builder(
            ChargingSchedule::new_blocking_active()
                .add_period(ChargingPeriod {
                    start: period1_start.and_utc().into(),
                    duration_s: period1_duration.num_seconds() as _,
                    power_limit: period1_limit,
                })
                .add_period(ChargingPeriod {
                    start: period2_start.and_utc().into(),
                    duration_s: period2_duration.num_seconds() as _,
                    power_limit: MAX_POWER,
                }),
        )
        .set_schedule_instant(set_schedule_instant.and_utc())
        .build_charging_schedule();

        assert_eq!(
            charging_schedule,
            vec![
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: 0,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: period1_start_delta.num_seconds() as _,
                    limit: period1_limit as _,
                    number_phases: Some(1),
                },
                ocpp_v16::ChargingSchedulePeriod {
                    start_period: (period1_start_delta + period1_duration).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
            ],
        );
    }

    #[test]
    pub fn deadline() {
        use charge_system_common_model::bms;

        crate::tests::init();

        const INITIAL_SOC: f32 = 0.30;
        const TARGET_SOC: f32 = 0.60;
        const ENERGY_TO_ADD: u32 =
            ((TARGET_SOC - INITIAL_SOC) * bms::DEFAULT_CAPACITY as f32) as u32;
        const DURATION_S: u64 = ENERGY_TO_ADD as u64 * 60 * 60 / bms::MAX_AVAILABLE_POWER as u64;

        let now = Utc::now();

        let end_time_can_start_today = now + TimeDelta::seconds(DURATION_S as _);
        let end_time_can_not_start_today =
            now + TimeDelta::seconds(DURATION_S as _) - TimeDelta::minutes(10);

        // unknown initial SoC
        let bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .target_soc(TARGET_SOC)
            .build()
            .unwrap();
        let plan = ChargingPlan::Deadline {
            deadline: end_time_can_start_today.into(),
            target_soc: TARGET_SOC,
            power_limit: bms::MAX_POWER,
        };
        assert!(ChargingSchedule::try_from_charging_plan(plan, &bms).is_err());

        // known initial SoC, can start today
        let bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .initial_soc(INITIAL_SOC)
            .target_soc(TARGET_SOC)
            .build()
            .unwrap();
        let plan = ChargingPlan::Deadline {
            deadline: end_time_can_start_today.into(),
            target_soc: TARGET_SOC,
            power_limit: bms::MAX_POWER,
        };
        let charging_schedule = ChargingSchedule::try_from_charging_plan(plan, &bms).unwrap();
        let start = (end_time_can_start_today - TimeDelta::seconds(DURATION_S as _)).into();
        let ref_schedule = ChargingSchedule {
            plan,
            periods: [(
                start,
                ChargingPeriod {
                    start,
                    duration_s: DURATION_S,
                    power_limit: MAX_POWER,
                },
            )]
            .into(),
            ..charging_schedule
        };
        assert_eq!(ref_schedule, charging_schedule);

        // known initial SOC, can't start today
        let bms = Bms::builder(bms::DEFAULT_CAPACITY)
            .initial_soc(INITIAL_SOC)
            .target_soc(TARGET_SOC)
            .build()
            .unwrap();
        let plan = ChargingPlan::Deadline {
            deadline: end_time_can_not_start_today.into(),
            target_soc: TARGET_SOC,
            power_limit: bms::MAX_POWER,
        };
        let charging_schedule = ChargingSchedule::try_from_charging_plan(plan, &bms).unwrap();
        // needs to start tomorrow
        let start = (end_time_can_not_start_today + TimeDelta::days(1)
            - TimeDelta::seconds(DURATION_S as _))
        .into();
        let ref_schedule = ChargingSchedule {
            plan,
            periods: [(
                start,
                ChargingPeriod {
                    start,
                    duration_s: DURATION_S,
                    power_limit: MAX_POWER,
                },
            )]
            .into(),
            ..charging_schedule
        };
        assert_eq!(ref_schedule, charging_schedule);
    }

    #[test]
    fn remaining() {
        crate::tests::init();

        const CONST_POWER_LOSS: u32 = 400;

        let today = Utc::now().date_naive();

        let period1_start = today
            .and_time(NaiveTime::from_hms_opt(1, 28, 00).unwrap())
            .and_utc();
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start = today
            .and_time(NaiveTime::from_hms_opt(13, 58, 00).unwrap())
            .and_utc();
        let period2_duration = TimeDelta::hours(1);

        let schedule = ChargingSchedule::new_blocking_active()
            .add_period(ChargingPeriod {
                start: period1_start.into(),
                duration_s: period1_duration.num_seconds() as _,
                power_limit: period1_limit,
            })
            .add_period(ChargingPeriod {
                start: period2_start.into(),
                duration_s: period2_duration.num_seconds() as _,
                power_limit: MAX_POWER,
            });

        let before_period1 = period1_start - TimeDelta::minutes(30);
        assert_eq!(
            RemainingDurationEnergy {
                duration: period1_duration + period2_duration,
                energy: ((period1_limit as u64 - CONST_POWER_LOSS as u64)
                    * (period1_duration.num_seconds() as u64)
                    / 60
                    / 60
                    + bms::MAX_AVAILABLE_POWER as u64 * (period2_duration.num_seconds() as u64)
                        / 60
                        / 60) as u32
            },
            schedule.remaining(before_period1, CONST_POWER_LOSS),
        );

        let during_period1 = period1_start + TimeDelta::minutes(30);
        let during_period1_dur = period1_start + period1_duration - during_period1;
        assert_eq!(
            RemainingDurationEnergy {
                duration: during_period1_dur + period2_duration,
                energy: ((period1_limit as u64 - CONST_POWER_LOSS as u64)
                    * (during_period1_dur.num_seconds() as u64)
                    / 60
                    / 60
                    + bms::MAX_AVAILABLE_POWER as u64 * (period2_duration.num_seconds() as u64)
                        / 60
                        / 60) as u32
            },
            schedule.remaining(during_period1, CONST_POWER_LOSS),
        );

        let between_periods = period1_start + period1_duration + TimeDelta::minutes(30);
        assert!(between_periods < period2_start);
        assert_eq!(
            RemainingDurationEnergy {
                duration: period2_duration,
                energy: (bms::MAX_AVAILABLE_POWER as u64 * (period2_duration.num_seconds() as u64)
                    / 60
                    / 60) as u32
            },
            schedule.remaining(between_periods, CONST_POWER_LOSS),
        );

        let during_period2 = period2_start + TimeDelta::minutes(30);
        let during_period2_dur = period2_start + period2_duration - during_period2;
        assert_eq!(
            RemainingDurationEnergy {
                duration: during_period2_dur,
                energy: (bms::MAX_AVAILABLE_POWER as u64
                    * (during_period2_dur.num_seconds() as u64)
                    / 60
                    / 60) as u32
            },
            schedule.remaining(during_period2, CONST_POWER_LOSS),
        );

        let after_period2 = period2_start + period2_duration + TimeDelta::minutes(30);
        assert!(
            schedule
                .remaining(after_period2, CONST_POWER_LOSS)
                .is_zero(),
        );
    }
}
