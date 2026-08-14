use std::collections::BTreeMap;

use anyhow::{Context, bail};
use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeDelta, Utc};
use log::{debug, info, warn};
use ocpp_rs::{
    datetime::DateTimeWrapper,
    v16::{
        call,
        data_types::{self, ChargingSchedulePeriod},
        enums::*,
    },
};

const DEFAULT_LIMIT: f64 = 7_400.0;
const DEFAULT_NB_OF_PHASES: i32 = 1;

pub struct ChargingProfile;
impl ChargingProfile {
    pub fn builder() -> ChargingProfileBuilder {
        ChargingProfileBuilder::default()
    }
}

#[derive(Debug)]
pub struct ChargingProfileBuilder {
    start_schedule_utc: DateTime<Utc>,
    periods: BTreeMap<NaiveDateTime, ChargingSchedulePeriodBuilder>,
}

impl Default for ChargingProfileBuilder {
    fn default() -> Self {
        let schedule_start = Local::now();
        debug!("Schedule start {schedule_start}");
        ChargingProfileBuilder {
            start_schedule_utc: schedule_start.to_utc(),
            periods: BTreeMap::new(),
        }
    }
}

impl ChargingProfileBuilder {
    // only intended at making tests predictable
    #[cfg(test)]
    fn with_start(schedule_start: NaiveDateTime) -> Self {
        debug!("Schedule start {schedule_start}");
        ChargingProfileBuilder {
            start_schedule_utc: schedule_start.and_local_timezone(Local).unwrap().to_utc(),
            periods: BTreeMap::new(),
        }
    }

    pub fn add_period(mut self, schedule_period: ChargingSchedulePeriodBuilder) -> Self {
        self.periods.insert(schedule_period.start, schedule_period);
        self
    }

    pub fn build_charging_schedule(self) -> Vec<ChargingSchedulePeriod> {
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
                        charging_schedule.push(data_types::ChargingSchedulePeriod {
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
                    charging_schedule.push(data_types::ChargingSchedulePeriod {
                        start_period: gap.num_seconds() as i32,
                        limit: 0.0,
                        number_phases: Some(DEFAULT_NB_OF_PHASES),
                    })
                }

                let start_period = start_utc - self.start_schedule_utc;
                debug!("Adding schedule period: {period:?}");
                charging_schedule.push(data_types::ChargingSchedulePeriod {
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
                charging_schedule.push(data_types::ChargingSchedulePeriod {
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
            charging_schedule.push(data_types::ChargingSchedulePeriod {
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
            cs_charging_profiles: data_types::ChargingProfile {
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
                charging_schedule: data_types::ChargingSchedule {
                    duration: None,
                    // FIXME seems to be ignored by the charging point
                    // need to clarify (for absolute profile kind):
                    // * does the schedule starts running relatively to submission?
                    // * does the schedule starts running when the transaction starts?
                    // the later would also explain the behaviour observed
                    // when requesting the schedule
                    start_schedule: Some(data_types::DateTimeWrapper::new(self.start_schedule_utc)),
                    charging_rate_unit: ChargingRateUnitType::W,
                    charging_schedule_period: self.build_charging_schedule(),
                    min_charging_rate: None,
                },
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChargingScheduleError {
    #[error("end {} is earlier than start {}", .end, .start)]
    EndEarlierThanStart { start: String, end: String },
}

#[derive(Debug)]
pub struct ChargingSchedulePeriodBuilder {
    start: NaiveDateTime,
    end: NaiveDateTime,
    limit: f64,
}

impl ChargingSchedulePeriodBuilder {
    #[allow(unused)]
    pub fn new(start: NaiveDateTime, end: NaiveDateTime) -> Result<Self, ChargingScheduleError> {
        if end < start {
            return Err(ChargingScheduleError::EndEarlierThanStart {
                start: start.to_string(),
                end: end.to_string(),
            });
        }
        Ok(ChargingSchedulePeriodBuilder {
            limit: DEFAULT_LIMIT,
            start,
            end,
        })
    }

    #[allow(unused)]
    pub fn with_duration(start: NaiveDateTime, duration: TimeDelta) -> Self {
        ChargingSchedulePeriodBuilder {
            limit: DEFAULT_LIMIT,
            start,
            end: start + duration,
        }
    }

    #[allow(unused)]
    pub fn starting_ending_today(
        start_time: NaiveTime,
        end_time: NaiveTime,
    ) -> Result<Self, ChargingScheduleError> {
        if end_time < start_time {
            return Err(ChargingScheduleError::EndEarlierThanStart {
                start: start_time.to_string(),
                end: end_time.to_string(),
            });
        }
        let today = Local::now().date_naive();
        Ok(ChargingSchedulePeriodBuilder {
            limit: DEFAULT_LIMIT,
            start: NaiveDateTime::new(today, start_time),
            end: NaiveDateTime::new(today, end_time),
        })
    }

    #[allow(unused)]
    pub fn starting_today_with_duration(start_time: NaiveTime, duration: TimeDelta) -> Self {
        let today = Local::now().date_naive();
        let start = NaiveDateTime::new(today, start_time);
        ChargingSchedulePeriodBuilder {
            limit: DEFAULT_LIMIT,
            start,
            end: start + duration,
        }
    }

    #[allow(unused)]
    pub fn starting_ending_tomorrow(
        start_time: NaiveTime,
        end_time: NaiveTime,
    ) -> Result<Self, ChargingScheduleError> {
        if end_time < start_time {
            return Err(ChargingScheduleError::EndEarlierThanStart {
                start: start_time.to_string(),
                end: end_time.to_string(),
            });
        }
        let tomorrow = Local::now().date_naive() + TimeDelta::days(1);
        Ok(ChargingSchedulePeriodBuilder {
            limit: DEFAULT_LIMIT,
            start: NaiveDateTime::new(tomorrow, start_time),
            end: NaiveDateTime::new(tomorrow, end_time),
        })
    }

    #[allow(unused)]
    pub fn starting_tomorrow_with_duration(start_time: NaiveTime, duration: TimeDelta) -> Self {
        let tomorrow = Local::now().date_naive() + TimeDelta::days(1);
        let start = NaiveDateTime::new(tomorrow, start_time);
        ChargingSchedulePeriodBuilder {
            limit: DEFAULT_LIMIT,
            start,
            end: start + duration,
        }
    }

    #[allow(unused)]
    pub fn starting_today_ending_tomorrow(start_time: NaiveTime, end_time: NaiveTime) -> Self {
        let today = Local::now().date_naive();
        let tomorrow = Local::now().date_naive() + TimeDelta::days(1);
        ChargingSchedulePeriodBuilder {
            limit: DEFAULT_LIMIT,
            start: NaiveDateTime::new(today, start_time),
            end: NaiveDateTime::new(tomorrow, end_time),
        }
    }

    #[allow(unused)]
    pub fn limit(mut self, limit: f64) -> Self {
        self.limit = limit;
        self
    }
}

#[allow(unused)]
pub fn build_set_charging_profile(
    start: NaiveDateTime,
    stop: NaiveDateTime,
    limit: f32,
) -> anyhow::Result<call::SetChargingProfile> {
    if stop <= start {
        bail!("expecting stop after start");
    }

    let start = start.and_local_timezone(Local).unwrap();
    let start = start.to_utc();

    let stop = stop.and_local_timezone(Local).unwrap();
    let stop = stop.to_utc();

    // FIXME use start day @ 00:00:00 instead
    let now = Utc::now();

    let start_charge_s: u32 = (start - now)
        .num_seconds()
        .try_into()
        .context("start before now")?;
    if start_charge_s > (i32::MAX as u32) {
        bail!("start is too far away");
    }

    let end_charge_s: u32 = (start_charge_s as i64 + (stop - start).num_seconds())
        .try_into()
        .context("stop is too far away")?;
    if end_charge_s > (i32::MAX as u32) {
        bail!("start is too far away");
    }

    Ok(call::SetChargingProfile {
        connector_id: 0,
        cs_charging_profiles: data_types::ChargingProfile {
            charging_profile_id: 0, // FIXME keep track of this
            transaction_id: None,
            stack_level: 0, // highest gets highest priority
            charging_profile_purpose: ChargingProfilePurposeType::ChargePointMaxProfile,
            charging_profile_kind: ChargingProfileKindType::Absolute,
            recurrency_kind: None,
            valid_from: None,
            valid_to: None,
            charging_schedule: data_types::ChargingSchedule {
                duration: None,
                start_schedule: Some(DateTimeWrapper::new(now)),
                charging_rate_unit: ChargingRateUnitType::W,
                charging_schedule_period: vec![
                    data_types::ChargingSchedulePeriod {
                        start_period: 0,
                        limit: 0.0,
                        number_phases: None,
                    },
                    data_types::ChargingSchedulePeriod {
                        start_period: start_charge_s as _,
                        limit,
                        number_phases: None,
                    },
                    data_types::ChargingSchedulePeriod {
                        start_period: end_charge_s as _,
                        limit: 0.0,
                        number_phases: None,
                    },
                ],
                min_charging_rate: None,
            },
        },
    })
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
    // FIXME signature + add an intermediate function to access the schedule
    // without building the call
    pub fn to_set_charging_profile(&self, bms: &crate::Bms) -> Option<call::SetChargingProfile> {
        // FIXME make off peak period configurable
        use ChargingPlan::*;
        match self {
            OffPeakToday => Some(
                ChargingProfile::builder()
                    .add_period(
                        ChargingSchedulePeriodBuilder::starting_ending_today(
                            NaiveTime::from_hms_opt(1, 28, 00).unwrap(),
                            NaiveTime::from_hms_opt(6, 58, 00).unwrap(),
                        )
                        .unwrap(),
                    )
                    .add_period(
                        ChargingSchedulePeriodBuilder::starting_ending_today(
                            NaiveTime::from_hms_opt(13, 58, 00).unwrap(),
                            NaiveTime::from_hms_opt(16, 28, 00).unwrap(),
                        )
                        .unwrap(),
                    )
                    .build(),
            ),
            OffPeakTomorrow => Some(
                ChargingProfile::builder()
                    .add_period(
                        ChargingSchedulePeriodBuilder::starting_ending_tomorrow(
                            NaiveTime::from_hms_opt(1, 28, 00).unwrap(),
                            NaiveTime::from_hms_opt(6, 58, 00).unwrap(),
                        )
                        .unwrap(),
                    )
                    .add_period(
                        ChargingSchedulePeriodBuilder::starting_ending_tomorrow(
                            NaiveTime::from_hms_opt(13, 58, 00).unwrap(),
                            NaiveTime::from_hms_opt(16, 28, 00).unwrap(),
                        )
                        .unwrap(),
                    )
                    .build(),
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
                    bms.initial_soc.unwrap_or_default() * 100.0,
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
                    ChargingProfile::builder()
                        .add_period(ChargingSchedulePeriodBuilder::with_duration(
                            start.naive_local(),
                            duration,
                        ))
                        .build(),
                )
            }
            NoLimit => None,
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

        let start_schedule = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = start_schedule.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_end = period1_start + period1_duration;
        let period1_limit = 5.0;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = start_schedule.time() + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);
        let period2_end = period2_start + period2_duration;

        let charging_schedule_today_hours = ChargingProfileBuilder::with_start(start_schedule)
            .add_period(
                ChargingSchedulePeriodBuilder::starting_ending_today(period1_start, period1_end)
                    .unwrap()
                    .limit(period1_limit),
            )
            .add_period(
                ChargingSchedulePeriodBuilder::starting_ending_today(period2_start, period2_end)
                    .unwrap(),
            )
            .build_charging_schedule();

        let charging_schedule_today_duration = ChargingProfileBuilder::with_start(start_schedule)
            .add_period(
                ChargingSchedulePeriodBuilder::starting_today_with_duration(
                    period1_start,
                    period1_duration,
                )
                .limit(period1_limit),
            )
            .add_period(ChargingSchedulePeriodBuilder::starting_today_with_duration(
                period2_start,
                period2_duration,
            ))
            .build_charging_schedule();

        assert_eq!(
            charging_schedule_today_hours,
            charging_schedule_today_duration
        );

        assert_eq!(
            charging_schedule_today_hours,
            vec![
                ChargingSchedulePeriod {
                    start_period: 0,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: period1_start_delta.num_seconds() as _,
                    limit: period1_limit as _,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: (period1_start_delta + period1_duration).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: period2_start_delta.num_seconds() as _,
                    limit: DEFAULT_LIMIT as _,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
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

        let start_schedule = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = start_schedule.time() - period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5.0;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = start_schedule.time() + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = ChargingProfileBuilder::with_start(start_schedule)
            .add_period(
                ChargingSchedulePeriodBuilder::starting_today_with_duration(
                    period1_start,
                    period1_duration,
                )
                .limit(period1_limit),
            )
            .add_period(ChargingSchedulePeriodBuilder::starting_today_with_duration(
                period2_start,
                period2_duration,
            ))
            .build_charging_schedule();

        assert_eq!(
            charging_schedule,
            vec![
                ChargingSchedulePeriod {
                    start_period: 0,
                    limit: period1_limit as _,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: (period1_duration - period1_start_delta).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: period2_start_delta.num_seconds() as _,
                    limit: DEFAULT_LIMIT as _,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
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

        let start_schedule = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(3);
        let period1_start = start_schedule.time() - period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5.0;

        let period2_start_delta = TimeDelta::hours(6);
        let period2_start = start_schedule.time() + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = ChargingProfileBuilder::with_start(start_schedule)
            .add_period(
                ChargingSchedulePeriodBuilder::starting_today_with_duration(
                    period1_start,
                    period1_duration,
                )
                .limit(period1_limit),
            )
            .add_period(ChargingSchedulePeriodBuilder::starting_today_with_duration(
                period2_start,
                period2_duration,
            ))
            .build_charging_schedule();

        assert_eq!(
            charging_schedule,
            vec![
                ChargingSchedulePeriod {
                    start_period: 0,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: period2_start_delta.num_seconds() as _,
                    limit: DEFAULT_LIMIT as _,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
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

        let start_schedule = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = start_schedule.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5.0;

        let period2_start_delta = TimeDelta::hours(2);
        let period2_start = start_schedule.time() + period2_start_delta;
        let period2_duration = TimeDelta::hours(3);

        let charging_schedule = ChargingProfileBuilder::with_start(start_schedule)
            .add_period(
                ChargingSchedulePeriodBuilder::starting_today_with_duration(
                    period1_start,
                    period1_duration,
                )
                .limit(period1_limit),
            )
            .add_period(ChargingSchedulePeriodBuilder::starting_today_with_duration(
                period2_start,
                period2_duration,
            ))
            .build_charging_schedule();

        assert_eq!(
            charging_schedule,
            vec![
                ChargingSchedulePeriod {
                    start_period: 0,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: period1_start_delta.num_seconds() as _,
                    limit: period1_limit as _,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: (period1_start_delta + period1_duration).num_seconds() as _,
                    limit: DEFAULT_LIMIT as _,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
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

        let start_schedule = NaiveDateTime::new(
            Local::now().date_naive(),
            NaiveTime::from_hms_opt(12, 00, 00).unwrap(),
        );

        let period1_start_delta = TimeDelta::hours(1);
        let period1_start = start_schedule.time() + period1_start_delta;
        let period1_duration = TimeDelta::hours(2);
        let period1_limit = 5.0;

        let period2_start_delta = TimeDelta::hours(2);
        let period2_start = start_schedule.time() + period2_start_delta;
        let period2_duration = TimeDelta::minutes(30);

        let charging_schedule = ChargingProfileBuilder::with_start(start_schedule)
            .add_period(
                ChargingSchedulePeriodBuilder::starting_today_with_duration(
                    period1_start,
                    period1_duration,
                )
                .limit(period1_limit),
            )
            .add_period(ChargingSchedulePeriodBuilder::starting_today_with_duration(
                period2_start,
                period2_duration,
            ))
            .build_charging_schedule();

        assert_eq!(
            charging_schedule,
            vec![
                ChargingSchedulePeriod {
                    start_period: 0,
                    limit: 0.0,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: period1_start_delta.num_seconds() as _,
                    limit: period1_limit as _,
                    number_phases: Some(1),
                },
                ChargingSchedulePeriod {
                    start_period: (period1_start_delta + period1_duration).num_seconds() as _,
                    limit: 0.0,
                    number_phases: Some(1),
                },
            ],
        );
    }
}
