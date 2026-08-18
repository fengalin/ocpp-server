use std::{
    collections::BTreeMap,
    fmt::{self, Write},
};

use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeDelta, Utc};
use log::{debug, info, warn};
use ocpp_rs::v16::{call, data_types as ocpp_v16, enums::*};

const DEFAULT_LIMIT: f64 = 7_400.0;
const DEFAULT_NB_OF_PHASES: i32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ChargingScheduleError {
    #[error("end {} is earlier than start {}", .end, .start)]
    EndEarlierThanStart { start: String, end: String },
}

#[derive(Debug)]
pub struct ChargingSchedulePeriod {
    start: NaiveDateTime,
    end: NaiveDateTime,
    limit: f64,
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
        f.write_fmt(format_args!("{:.0}", self.limit))?;
        f.write_char(')')
    }
}

#[derive(Debug, Default)]
pub struct ChargingSchedule(BTreeMap<NaiveDateTime, ChargingSchedulePeriod>);
impl ChargingSchedule {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_period(mut self, period: ChargingSchedulePeriod) -> Self {
        self.0.insert(period.start, period);
        self
    }

    pub fn periods(&self) -> impl Iterator<Item = &ChargingSchedulePeriod> {
        self.0.values()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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
    pub fn limit(mut self, limit: f64) -> Self {
        self.0.limit = limit;
        self
    }

    pub fn build(self) -> ChargingSchedulePeriod {
        self.0
    }
}

#[derive(Debug, Copy, Clone)]
pub enum ChargingPlan {
    OffPeakToday,
    OffPeakTomorrow,
    // FIXME add power limit
    ReachSocCapBefore { end_time: NaiveTime },
    NoLimit,
}

impl ChargingPlan {
    pub fn to_charging_schedule(&self, bms: &crate::Bms) -> Option<ChargingSchedule> {
        // FIXME make off peak period configurable
        use ChargingPlan::*;
        match self {
            OffPeakToday => Some(
                ChargingSchedule::new()
                    .add_period(
                        ChargingSchedulePeriod::builder_starting_today(
                            NaiveTime::from_hms_opt(1, 28, 00).unwrap(),
                        )
                        .end_time(NaiveTime::from_hms_opt(6, 58, 00).unwrap())
                        .build(),
                    )
                    .add_period(
                        ChargingSchedulePeriod::builder_starting_today(
                            NaiveTime::from_hms_opt(13, 58, 00).unwrap(),
                        )
                        .end_time(NaiveTime::from_hms_opt(16, 28, 00).unwrap())
                        .build(),
                    ),
            ),
            OffPeakTomorrow => Some(
                ChargingSchedule::new()
                    .add_period(
                        ChargingSchedulePeriod::builder_starting_tomorrow(
                            NaiveTime::from_hms_opt(1, 28, 00).unwrap(),
                        )
                        .end_time(NaiveTime::from_hms_opt(6, 58, 00).unwrap())
                        .build(),
                    )
                    .add_period(
                        ChargingSchedulePeriod::builder_starting_tomorrow(
                            NaiveTime::from_hms_opt(13, 58, 00).unwrap(),
                        )
                        .end_time(NaiveTime::from_hms_opt(16, 28, 00).unwrap())
                        .build(),
                    ),
            ),
            ReachSocCapBefore { end_time } => {
                let energy_to_add = bms.get_energy_to_add();
                // FIXME use power limit from args
                let available_power = DEFAULT_LIMIT - bms.constant_power_loss as f64;
                // FIXME check in args
                assert!(available_power > 0.0);
                // FIXME would also need a ponderation in case of variations due to DPM
                let duration_s = energy_to_add / available_power * 60.0 * 60.0;
                let energy_needed = duration_s * DEFAULT_LIMIT / 60.0 / 60.0;
                // FIXME add extra duration as we get closer to 100% SoC

                let now = Local::now();
                let today = now.date_naive();
                let mut start_day = today;
                let mut start;
                let duration = TimeDelta::seconds(duration_s as _);
                info!(
                    "preparing schedule for SoC {:.0} % -> {:.0} %\n\
                    \tenergy to add: {:.1} kWh\n\
                    \tenergy needed: {:.1} kWh (loss {:.1} %)\n\
                    \tduration {} mn",
                    bms.reference_soc.unwrap_or_default() * 100.0,
                    bms.soc_cap.expect("defined at this stage") * 100.0,
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
        let set_charging_profile_start = Local::now();
        debug!("SetCharingProfile start {set_charging_profile_start}");
        SetChargingProfileBuilder {
            start_schedule_utc: set_charging_profile_start.to_utc(),
            periods: schedule.0,
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

    fn init() {
        use std::sync::Once;
        static LOGGER: Once = Once::new();
        LOGGER.call_once(|| {
            env_logger::builder()
                .format_source_path(true)
                .format_line_number(true)
                .try_init()
                .unwrap();
        });
    }

    #[test]
    pub fn disjointed_in_the_future() {
        init();

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_end = period1_start + period1_duration;
        let period1_limit = 5.0;

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
        init();

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant.time() - period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5.0;

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
        init();

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(3);
        let period1_start = set_schedule_instant.time() - period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5.0;

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
        init();

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5.0;

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
        init();

        let set_schedule_instant = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = set_schedule_instant.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5.0;

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
}
