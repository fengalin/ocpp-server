use std::{
    collections::BTreeMap,
    fmt::{self, Write},
};

use chrono::{DateTime, Duration, Local, NaiveDateTime, NaiveTime, TimeDelta, Utc};
use log::{debug, error, info, warn};
use ocpp_rs::v16::{call, data_types as ocpp_v16, enums::*};

use crate::Database;

const DEFAULT_LIMIT: f64 = 7_400.0;
const DEFAULT_NB_OF_PHASES: i32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ChargingScheduleError {
    #[error("end {} is earlier than start {}", .end, .start)]
    EndEarlierThanStart { start: String, end: String },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargingSchedulePeriod {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub limit: f64,
}
impl ChargingSchedulePeriod {
    pub fn builder(start: NaiveDateTime) -> ChargingSchedulePeriodBuilderNoEnd {
        ChargingSchedulePeriodBuilderNoEnd {
            limit: DEFAULT_LIMIT,
            start,
        }
    }
    pub fn builder_starting_today(start_time: NaiveTime) -> ChargingSchedulePeriodBuilderNoEnd {
        let today = Local::now().date_naive();
        ChargingSchedulePeriodBuilderNoEnd {
            limit: DEFAULT_LIMIT,
            start: NaiveDateTime::new(today, start_time),
        }
    }
    pub fn builder_starting_tomorrow(start_time: NaiveTime) -> ChargingSchedulePeriodBuilderNoEnd {
        let tomorrow = Local::now().date_naive() + TimeDelta::days(1);
        ChargingSchedulePeriodBuilderNoEnd {
            limit: DEFAULT_LIMIT,
            start: NaiveDateTime::new(tomorrow, start_time),
        }
    }
}

impl fmt::Display for ChargingSchedulePeriod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.start.to_string())?;
        f.write_str(" -> ")?;
        f.write_str(&self.end.to_string())?;
        f.write_str(" (")?;
        f.write_fmt(format_args!("{:.3} kW", self.limit / 1_000.0))?;
        f.write_char(')')
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct OutstandingDurationEnergy {
    pub duration: Duration,
    pub energy: f64,
}
impl OutstandingDurationEnergy {
    fn zero() -> Self {
        Default::default()
    }

    pub fn is_zero(&self) -> bool {
        *self == OutstandingDurationEnergy::zero()
    }

    fn add(&mut self, delta: TimeDelta, active_power: f64) {
        assert!(delta.num_seconds() > 0);
        self.duration += delta;
        self.energy += active_power * delta.num_seconds() as f64 / 60.0 / 60.0;
    }
}

impl fmt::Display for OutstandingDurationEnergy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return f.write_str("permanent 0 W (blocking)");
        }

        let total_secs = self.duration.num_seconds();
        let total_mins = total_secs / 60;
        let hours = total_mins / 60;
        let mins = total_mins % 60;
        let secs = total_secs % 60;
        if hours > 0 {
            f.write_fmt(format_args!("{hours}:{mins:02}:{secs:02}",))?;
        } else {
            f.write_fmt(format_args!("{mins:02}:{secs:02}",))?;
        }

        f.write_str(", +")?;
        f.write_fmt(format_args!("{:.3} kWh", self.energy / 1_000.0))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChargingSchedule {
    pub id: i32,
    pub set_time: DateTime<Utc>,
    pub state: ChargingScheduleState,
    pub periods: BTreeMap<NaiveDateTime, ChargingSchedulePeriod>,
}

impl ChargingSchedule {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_inactive() -> Self {
        ChargingSchedule {
            state: ChargingScheduleState::Inactive,
            ..Default::default()
        }
    }

    pub fn inactivate(&mut self) {
        self.state = ChargingScheduleState::Inactive;
        Database::get().inactivate_charging_schedule(self.id);
    }

    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    pub fn add_period(mut self, period: ChargingSchedulePeriod) -> Self {
        self.periods.insert(period.start, period);
        self
    }

    pub fn periods(&self) -> impl Iterator<Item = &ChargingSchedulePeriod> {
        self.periods.values()
    }

    pub fn is_empty(&self) -> bool {
        self.periods.is_empty()
    }

    pub fn reset_set_time(&mut self) {
        self.set_time = Utc::now();
    }

    // FIXME energy that will be added also depends on the SoC and DPM profile
    pub fn outstanding(
        &self,
        from_instant: NaiveDateTime,
        constant_power_loss: u16,
    ) -> OutstandingDurationEnergy {
        let mut rem = OutstandingDurationEnergy::default();
        if !self.is_active() {
            return rem;
        }
        for period in self.periods.values() {
            if from_instant >= period.end {
                continue;
            }
            if from_instant > period.start && from_instant < period.end {
                rem.add(
                    period.end - from_instant,
                    period.limit - constant_power_loss as f64,
                );
                continue;
            }
            rem.add(
                period.end - period.start,
                period.limit - constant_power_loss as f64,
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
            state: ChargingScheduleState::Active,
            periods: Default::default(),
        }
    }
}

impl fmt::Display for ChargingSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "id: {}, state {}, set time: {}",
            self.id,
            self.state,
            self.set_time.with_timezone(&Local),
        ))?;
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
pub struct ChargingSchedulePeriodBuilderNoEnd {
    start: NaiveDateTime,
    limit: f64,
}
impl ChargingSchedulePeriodBuilderNoEnd {
    pub fn limit(mut self, limit: f64) -> Self {
        self.limit = limit;
        self
    }

    pub fn duration(self, duration: TimeDelta) -> ChargingSchedulePeriodBuilder {
        ChargingSchedulePeriodBuilder(ChargingSchedulePeriod {
            limit: self.limit,
            start: self.start,
            end: self.start + duration,
        })
    }

    pub fn end(
        self,
        end: NaiveDateTime,
    ) -> Result<ChargingSchedulePeriodBuilder, ChargingScheduleError> {
        if end < self.start {
            return Err(ChargingScheduleError::EndEarlierThanStart {
                start: self.start.to_string(),
                end: end.to_string(),
            });
        }
        Ok(ChargingSchedulePeriodBuilder(ChargingSchedulePeriod {
            limit: self.limit,
            start: self.start,
            end,
        }))
    }

    pub fn end_time(self, end_time: NaiveTime) -> ChargingSchedulePeriodBuilder {
        let end_day = if self.start.time() < end_time {
            self.start.date()
        } else {
            self.start.date() + TimeDelta::days(1)
        };
        ChargingSchedulePeriodBuilder(ChargingSchedulePeriod {
            limit: self.limit,
            start: self.start,
            end: NaiveDateTime::new(end_day, end_time),
        })
    }
}

#[derive(Debug)]
pub struct ChargingSchedulePeriodBuilder(ChargingSchedulePeriod);
impl ChargingSchedulePeriodBuilder {
    #[allow(unused)]
    pub fn limit(mut self, limit: u32) -> Self {
        self.0.limit = limit as f64;
        self
    }

    pub fn build(self) -> ChargingSchedulePeriod {
        self.0
    }
}

#[derive(Debug, Copy, Clone)]
pub enum ChargingPlan {
    Blocking,
    OffPeakToday {
        power_limit: u32,
    },
    OffPeakTomorrow {
        power_limit: u32,
    },
    ReachSocCapBefore {
        end_time: NaiveTime,
        power_limit: u32,
    },
    NoLimit,
}

impl ChargingPlan {
    pub fn to_charging_schedule(&self, bms: &crate::Bms) -> Option<ChargingSchedule> {
        // FIXME make off peak period configurable
        use ChargingPlan::*;
        match self {
            Blocking => Some(ChargingSchedule::new()),
            OffPeakToday { power_limit } => Some(
                ChargingSchedule::new()
                    .add_period(
                        ChargingSchedulePeriod::builder_starting_today(
                            NaiveTime::from_hms_opt(1, 28, 00).unwrap(),
                        )
                        .end_time(NaiveTime::from_hms_opt(6, 58, 00).unwrap())
                        .limit(*power_limit)
                        .build(),
                    )
                    .add_period(
                        ChargingSchedulePeriod::builder_starting_today(
                            NaiveTime::from_hms_opt(13, 58, 00).unwrap(),
                        )
                        .end_time(NaiveTime::from_hms_opt(16, 28, 00).unwrap())
                        .limit(*power_limit)
                        .build(),
                    ),
            ),
            OffPeakTomorrow { power_limit } => Some(
                ChargingSchedule::new()
                    .add_period(
                        ChargingSchedulePeriod::builder_starting_tomorrow(
                            NaiveTime::from_hms_opt(1, 28, 00).unwrap(),
                        )
                        .end_time(NaiveTime::from_hms_opt(6, 58, 00).unwrap())
                        .limit(*power_limit)
                        .build(),
                    )
                    .add_period(
                        ChargingSchedulePeriod::builder_starting_tomorrow(
                            NaiveTime::from_hms_opt(13, 58, 00).unwrap(),
                        )
                        .end_time(NaiveTime::from_hms_opt(16, 28, 00).unwrap())
                        .limit(*power_limit)
                        .build(),
                    ),
            ),
            ReachSocCapBefore {
                end_time,
                power_limit,
            } => {
                let Some(energy_to_add) = bms.energy_to_soc(bms.soc_cap()) else {
                    error!(
                        "couldn't compute energy to add for {} to {}",
                        bms.current_soc,
                        bms.soc_cap(),
                    );
                    return None;
                };

                let available_power =
                    power_limit.saturating_sub(bms.constant_power_loss as u32) as f64;
                // FIXME check in args
                assert!(available_power > 0.0);
                // FIXME would also need a ponderation in case of variations due to DPM
                let duration_s = energy_to_add / available_power * 60.0 * 60.0;
                let energy_needed = duration_s * (*power_limit as f64) / 60.0 / 60.0;
                // FIXME add extra duration as we get closer to 100% SoC

                let now = Local::now();
                let today = now.date_naive();
                let mut start_day = today;
                let mut start;
                let duration = TimeDelta::seconds(duration_s as _);
                info!(
                    "preparing schedule for SoC {} to {}\n\
                    \tenergy to add: {:.3} kWh\n\
                    \tenergy needed: {:.3} kWh (loss {:.1} %)\n\
                    \tduration {} mn",
                    bms.current_soc,
                    bms.soc_cap(),
                    energy_to_add / 1_000.0,
                    energy_needed / 1_000.0,
                    (1.0 - (energy_to_add / energy_needed)) * 100.0,
                    duration.num_minutes(),
                );
                // compute in local time in order to avoid difference due to dst
                loop {
                    start = (NaiveDateTime::new(start_day, *end_time) - duration)
                        .and_local_timezone(Local)
                        .unwrap();

                    if start > now + TimeDelta::minutes(2) {
                        // FIXME added a safety margin, needs more thinking + const / conf
                        break;
                    }
                    // charging would have needed to start earlier => select next day
                    start_day += TimeDelta::days(1);
                }
                if start_day != today {
                    warn!("charging schedule can't start today");
                }

                info!("charging schedule: {start} to {}", start + duration);

                // FIXME add a charging plan to span off teak periods

                Some(
                    ChargingSchedule::new().add_period(
                        ChargingSchedulePeriod::builder(start.naive_local())
                            .duration(duration)
                            .build(),
                    ),
                )
            }
            NoLimit => None,
        }
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
            start_schedule_utc: schedule.set_time,
            periods: schedule.periods,
        }
    }
}

#[derive(Debug)]
pub struct SetChargingProfileBuilder {
    start_schedule_utc: DateTime<Utc>,
    periods: BTreeMap<NaiveDateTime, ChargingSchedulePeriod>,
}

impl SetChargingProfileBuilder {
    // only intended at making tests predictable
    #[cfg(test)]
    fn set_schedule_instant(mut self, schedule_start: NaiveDateTime) -> Self {
        debug!("Schedule start {schedule_start}");
        self.start_schedule_utc = schedule_start.and_local_timezone(Local).unwrap().to_utc();
        self
    }

    pub fn build_charging_schedule(self) -> Vec<ocpp_v16::ChargingSchedulePeriod> {
        let mut charging_schedule = vec![];
        let mut last_end_utc = None;
        for period in self.periods.values() {
            let start_utc = period.start.and_local_timezone(Local).unwrap().to_utc();
            let end_utc = period.end.and_local_timezone(Local).unwrap().to_utc();

            if last_end_utc.is_none() {
                // first period
                if start_utc >= self.start_schedule_utc {
                    // first period starts in the future
                    last_end_utc = Some(self.start_schedule_utc);
                    // proceed as a regular period
                } else {
                    // first period starts in the past
                    if end_utc > self.start_schedule_utc {
                        // first period ends in the future
                        debug!(
                            "Adding truncated first schedule period: {period:?},\
                            starting at {}",
                            self.start_schedule_utc.with_timezone(&Local)
                        );
                        charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                            start_period: 0,
                            limit: period.limit as _,
                            number_phases: Some(DEFAULT_NB_OF_PHASES),
                        });
                        last_end_utc = Some(end_utc);
                    } else {
                        debug!("Skipping first schedule period entirely in the past: {period:?}");
                    }

                    continue;
                }
            }
            // else not first period

            let Some(ref mut last_end_utc) = last_end_utc else {
                unreachable!("checked / assigned above");
            };
            if start_utc >= *last_end_utc {
                if start_utc > *last_end_utc {
                    // add a gap
                    let gap = *last_end_utc - self.start_schedule_utc;
                    debug!(
                        "Adding schedule gap: {} seconds",
                        (start_utc - *last_end_utc).num_seconds()
                    );
                    charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                        start_period: gap.num_seconds() as i32,
                        limit: 0.0,
                        number_phases: Some(DEFAULT_NB_OF_PHASES),
                    })
                }

                let start_period = start_utc - self.start_schedule_utc;
                debug!("Adding schedule period: {period:?}");
                charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                    start_period: start_period.num_seconds() as i32,
                    limit: period.limit as _,
                    number_phases: Some(DEFAULT_NB_OF_PHASES),
                });
            } else if end_utc > *last_end_utc {
                // this period is set to start before last period ends
                // and it ends after last period's end
                debug!(
                    "Adding truncated schedule period: {period:?}, starting at {}",
                    last_end_utc.with_timezone(&Local)
                );
                let last_end_period = *last_end_utc - self.start_schedule_utc;
                charging_schedule.push(ocpp_v16::ChargingSchedulePeriod {
                    start_period: last_end_period.num_seconds() as i32,
                    limit: period.limit as _,
                    number_phases: Some(DEFAULT_NB_OF_PHASES),
                });
            } else {
                debug!(
                    "Skipping schedule period: {period:?},\
                        overlapping previous period",
                );
            }

            if end_utc > *last_end_utc {
                *last_end_utc = end_utc;
            }
        }

        let last_end_utc = last_end_utc.unwrap_or(self.start_schedule_utc);

        let last_end_start = last_end_utc - self.start_schedule_utc;
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
                    start_schedule: Some(ocpp_v16::DateTimeWrapper::new(self.start_schedule_utc)),
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
    use super::*;

    #[test]
    pub fn disjointed_in_the_future() {
        crate::tests::init();

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_end = period1_start + period1_duration;
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = set_schedule_instant.time() + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);
        let period2_end = period2_start + period2_duration;

        let charging_schedule_today_hours = SetChargingProfile::builder(
            ChargingSchedule::new()
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period1_start)
                        .end_time(period1_end)
                        .limit(period1_limit)
                        .build(),
                )
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period2_start)
                        .end_time(period2_end)
                        .build(),
                ),
        )
        .set_schedule_instant(set_schedule_instant)
        .build_charging_schedule();

        let charging_schedule_today_duration = SetChargingProfile::builder(
            ChargingSchedule::new()
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period1_start)
                        .duration(period1_duration)
                        .limit(period1_limit)
                        .build(),
                )
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period2_start)
                        .duration(period2_duration)
                        .build(),
                ),
        )
        .set_schedule_instant(set_schedule_instant)
        .build_charging_schedule();

        assert_eq!(
            charging_schedule_today_hours,
            charging_schedule_today_duration
        );

        assert_eq!(
            charging_schedule_today_hours,
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
                    limit: DEFAULT_LIMIT as _,
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

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant.time() - period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = set_schedule_instant.time() + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = SetChargingProfile::builder(
            ChargingSchedule::new()
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period1_start)
                        .duration(period1_duration)
                        .limit(period1_limit)
                        .build(),
                )
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period2_start)
                        .duration(period2_duration)
                        .build(),
                ),
        )
        .set_schedule_instant(set_schedule_instant)
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
                    limit: DEFAULT_LIMIT as _,
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

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(3);
        let period1_start = set_schedule_instant.time() - period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = set_schedule_instant.time() + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = SetChargingProfile::builder(
            ChargingSchedule::new()
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period1_start)
                        .duration(period1_duration)
                        .limit(period1_limit)
                        .build(),
                )
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period2_start)
                        .duration(period2_duration)
                        .build(),
                ),
        )
        .set_schedule_instant(set_schedule_instant)
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
                    limit: DEFAULT_LIMIT as _,
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

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(2);
        let period2_start = set_schedule_instant.time() + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = SetChargingProfile::builder(
            ChargingSchedule::new()
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period1_start)
                        .duration(period1_duration)
                        .limit(period1_limit)
                        .build(),
                )
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period2_start)
                        .duration(period2_duration)
                        .build(),
                ),
        )
        .set_schedule_instant(set_schedule_instant)
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
                    limit: DEFAULT_LIMIT as _,
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

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start_delta = TimeDelta::hours(2);
        let period2_start = set_schedule_instant.time() + period2_start_delta;
        let period2_duration = TimeDelta::minutes(30);

        let charging_schedule = SetChargingProfile::builder(
            ChargingSchedule::new()
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period1_start)
                        .duration(period1_duration)
                        .limit(period1_limit)
                        .build(),
                )
                .add_period(
                    ChargingSchedulePeriod::builder_starting_today(period2_start)
                        .duration(period2_duration)
                        .build(),
                ),
        )
        .set_schedule_instant(set_schedule_instant)
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
    pub fn reach_soc_before() {
        use crate::SoC;

        crate::tests::init();

        const BATTERY_CAPACITY: u32 = 48_100;
        const POWER_LIMIT: u32 = 7_400;
        const CONST_POWER_LOSS: u16 = 400;
        const INITIAL_SOC: f64 = 0.30;
        const SOC_CAP: f64 = 0.60;
        const ENERGY_TO_ADD: f64 = (SOC_CAP - INITIAL_SOC) * BATTERY_CAPACITY as f64;
        const DURATION_S: u64 = (60.0 * 60.0 * ENERGY_TO_ADD
            / (POWER_LIMIT - CONST_POWER_LOSS as u32) as f64)
            .round() as u64;
        const HOURS_NEEDED_CEIL: u64 = DURATION_S.div_ceil(60 * 60);

        let now = Local::now();

        let end_time_can_start_today = now + TimeDelta::hours(HOURS_NEEDED_CEIL as _);
        let end_time_can_not_start_today = now + TimeDelta::hours((HOURS_NEEDED_CEIL - 2) as _);

        // unknown initial SoC & SoC cap
        let bms = crate::Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS).build();
        let charging_schedule = ChargingPlan::ReachSocCapBefore {
            end_time: end_time_can_start_today.time(),
            power_limit: POWER_LIMIT,
        }
        .to_charging_schedule(&bms);
        assert_eq!(None, charging_schedule);

        // unknown initial SoC
        let bms = crate::Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .soc_cap(SOC_CAP)
            .build();
        let charging_schedule = ChargingPlan::ReachSocCapBefore {
            end_time: end_time_can_start_today.time(),
            power_limit: POWER_LIMIT,
        }
        .to_charging_schedule(&bms);
        assert_eq!(None, charging_schedule);

        // unknown initial SoC cap
        let bms = crate::Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(INITIAL_SOC))
            .build();
        let charging_schedule = ChargingPlan::ReachSocCapBefore {
            end_time: end_time_can_start_today.time(),
            power_limit: POWER_LIMIT,
        }
        .to_charging_schedule(&bms);
        assert_eq!(None, charging_schedule);

        // known initial SoC, can start today
        let bms = crate::Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(INITIAL_SOC))
            .soc_cap(SOC_CAP)
            .build();
        let charging_schedule = ChargingPlan::ReachSocCapBefore {
            end_time: end_time_can_start_today.time(),
            power_limit: POWER_LIMIT,
        }
        .to_charging_schedule(&bms)
        .unwrap();
        let mut ref_schedule = ChargingSchedule::new().add_period(
            ChargingSchedulePeriod::builder(
                end_time_can_start_today.naive_local() - TimeDelta::seconds(DURATION_S as _),
            )
            .duration(TimeDelta::seconds(DURATION_S as _))
            .limit(POWER_LIMIT)
            .build(),
        );
        ref_schedule.set_time = charging_schedule.set_time;
        assert_eq!(ref_schedule, charging_schedule,);

        // known initial SOC, can't start today
        let bms = crate::Bms::builder(BATTERY_CAPACITY, CONST_POWER_LOSS)
            .initial_soc(SoC::Absolute(INITIAL_SOC))
            .soc_cap(SOC_CAP)
            .build();
        let charging_schedule = ChargingPlan::ReachSocCapBefore {
            end_time: end_time_can_not_start_today.time(),
            power_limit: POWER_LIMIT,
        }
        .to_charging_schedule(&bms)
        .unwrap();
        let mut ref_schedule = ChargingSchedule::new().add_period(
            ChargingSchedulePeriod::builder(
                // needs to start tomorrow
                (end_time_can_not_start_today + TimeDelta::days(1)).naive_local()
                    - TimeDelta::seconds(DURATION_S as _),
            )
            .duration(TimeDelta::seconds(DURATION_S as _))
            .limit(POWER_LIMIT)
            .build(),
        );
        ref_schedule.set_time = charging_schedule.set_time;
        assert_eq!(ref_schedule, charging_schedule,);
    }

    #[test]
    fn persist_charging_schedule() {
        crate::tests::init();

        let period1_start = NaiveTime::from_hms_opt(1, 28, 00).unwrap();
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start = NaiveTime::from_hms_opt(13, 58, 00).unwrap();
        let period2_duration = TimeDelta::minutes(30);

        let mut schedule = ChargingSchedule::new()
            .add_period(
                ChargingSchedulePeriod::builder_starting_today(period1_start)
                    .duration(period1_duration)
                    .limit(period1_limit)
                    .build(),
            )
            .add_period(
                ChargingSchedulePeriod::builder_starting_today(period2_start)
                    .duration(period2_duration)
                    .build(),
            );

        let schedule_id = Database::get().add_new_charging_schedule(&schedule);
        schedule.id = schedule_id;

        let last_schedule = Database::get()
            .get_active_charging_schedule()
            .unwrap()
            .unwrap();

        assert_eq!(schedule, last_schedule);
    }

    #[test]
    fn outstanding() {
        crate::tests::init();

        const CONST_POWER_LOSS: u16 = 400;

        let period1_start = NaiveTime::from_hms_opt(1, 28, 00).unwrap();
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5_000;

        let period2_start = NaiveTime::from_hms_opt(13, 58, 00).unwrap();
        let period2_duration = TimeDelta::hours(1);

        let mut schedule = ChargingSchedule::new()
            .add_period(
                ChargingSchedulePeriod::builder_starting_today(period1_start)
                    .duration(period1_duration)
                    .limit(period1_limit)
                    .build(),
            )
            .add_period(
                ChargingSchedulePeriod::builder_starting_today(period2_start)
                    .duration(period2_duration)
                    .build(),
            );

        let today = Local::now().date_naive();
        let period1_start = NaiveDateTime::new(today, period1_start);
        let period2_start = NaiveDateTime::new(today, period2_start);

        let before_period1 = period1_start - TimeDelta::minutes(30);
        assert_eq!(
            OutstandingDurationEnergy {
                duration: period1_duration + period2_duration,
                energy: (period1_limit as f64 - CONST_POWER_LOSS as f64)
                    * (period1_duration.num_seconds() as f64)
                    / 60.0
                    / 60.0
                    + (DEFAULT_LIMIT - CONST_POWER_LOSS as f64)
                        * (period2_duration.num_seconds() as f64)
                        / 60.0
                        / 60.0
            },
            schedule.outstanding(before_period1, CONST_POWER_LOSS),
        );

        let during_period1 = period1_start + TimeDelta::minutes(30);
        let during_period1_dur = period1_start + period1_duration - during_period1;
        assert_eq!(
            OutstandingDurationEnergy {
                duration: during_period1_dur + period2_duration,
                energy: (period1_limit as f64 - CONST_POWER_LOSS as f64)
                    * (during_period1_dur.num_seconds() as f64)
                    / 60.0
                    / 60.0
                    + (DEFAULT_LIMIT - CONST_POWER_LOSS as f64)
                        * (period2_duration.num_seconds() as f64)
                        / 60.0
                        / 60.0
            },
            schedule.outstanding(during_period1, CONST_POWER_LOSS),
        );

        let between_periods = period1_start + period1_duration + TimeDelta::minutes(30);
        assert!(between_periods < period2_start);
        assert_eq!(
            OutstandingDurationEnergy {
                duration: period2_duration,
                energy: (DEFAULT_LIMIT - CONST_POWER_LOSS as f64)
                    * (period2_duration.num_seconds() as f64)
                    / 60.0
                    / 60.0
            },
            schedule.outstanding(between_periods, CONST_POWER_LOSS),
        );

        let during_period2 = period2_start + TimeDelta::minutes(30);
        let during_period2_dur = period2_start + period2_duration - during_period2;
        assert_eq!(
            OutstandingDurationEnergy {
                duration: during_period2_dur,
                energy: (DEFAULT_LIMIT - CONST_POWER_LOSS as f64)
                    * (during_period2_dur.num_seconds() as f64)
                    / 60.0
                    / 60.0
            },
            schedule.outstanding(during_period2, CONST_POWER_LOSS),
        );

        let after_period2 = period2_start + period2_duration + TimeDelta::minutes(30);
        assert!(
            schedule
                .outstanding(after_period2, CONST_POWER_LOSS)
                .is_zero(),
        );

        schedule.inactivate();
        assert!(
            schedule
                .outstanding(before_period1, CONST_POWER_LOSS)
                .is_zero(),
        );
    }
}
