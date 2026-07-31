use std::collections::BTreeMap;

use anyhow::{Context, bail};
use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeDelta, Utc};
use log::debug;
use ocpp_rs::{
    datetime::DateTimeWrapper,
    v16::{call, data_types, enums::*},
};

const DEFAULT_LIMIT: f32 = 7_400.0;
const DEFAULT_NB_OF_PHASES: i32 = 1;

#[derive(Debug)]
pub struct ChargingProfileBuilder {
    periods: BTreeMap<NaiveDateTime, ChargingSchedulePeriodBuild>,
}

struct ChargingScheduleState {
    // FIXME once actual behaviour is clarified,
    // this also needs to be fixed:
    // for an absolute schedule, the start_schedule should be
    // set in the past so as to make sure it doesn't start
    // if the car is connected before the first period is limit > 0
    start_schedule_utc: DateTime<Utc>,
    last_end_utc: DateTime<Utc>,
}

impl ChargingProfileBuilder {
    pub fn new() -> Self {
        ChargingProfileBuilder {
            periods: BTreeMap::new(),
        }
    }

    pub fn add(mut self, schedule_period: ChargingSchedulePeriodBuild) -> Self {
        self.periods.insert(schedule_period.start, schedule_period);
        self
    }

    pub fn build(self) -> Option<call::SetChargingProfile> {
        let mut charging_schedule = vec![];
        let mut charging_schedule_state = None;
        for period in self.periods.values() {
            let start_utc = period.start.and_local_timezone(Local).unwrap().to_utc();

            let state = charging_schedule_state.get_or_insert_with(|| {
                let schedule_start_utc = NaiveDateTime::new(
                    start_utc.date_naive(),
                    NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                )
                .and_utc();
                debug!("Schedule start {schedule_start_utc}");
                ChargingScheduleState {
                    start_schedule_utc: schedule_start_utc,
                    last_end_utc: schedule_start_utc,
                }
            });

            if start_utc > state.last_end_utc {
                // add a gap
                let gap = state.last_end_utc - state.start_schedule_utc;
                debug!(
                    "Adding schedule gap: {} seconds",
                    (start_utc - state.last_end_utc).num_seconds()
                );
                charging_schedule.push(data_types::ChargingSchedulePeriod {
                    start_period: gap.num_seconds() as i32,
                    limit: 0.0,
                    number_phases: Some(DEFAULT_NB_OF_PHASES),
                })
            }

            let start_period = start_utc - state.start_schedule_utc;
            debug!("Adding schedule period: {period:?}");
            charging_schedule.push(data_types::ChargingSchedulePeriod {
                start_period: start_period.num_seconds() as i32,
                limit: period.limit,
                number_phases: Some(DEFAULT_NB_OF_PHASES),
            });

            state.last_end_utc = period.end.and_local_timezone(Local).unwrap().to_utc();
        }

        let Some(state) = charging_schedule_state else {
            return None;
        };

        let last_end_start = state.last_end_utc - state.start_schedule_utc;
        if last_end_start.num_seconds() > 0 {
            charging_schedule.push(data_types::ChargingSchedulePeriod {
                start_period: last_end_start.num_seconds() as i32,
                limit: 0.0,
                number_phases: Some(DEFAULT_NB_OF_PHASES),
            });
        }

        debug!("resulting ChargingSchedule\n{charging_schedule:#?}");

        Some(call::SetChargingProfile {
            connector_id: 0,
            cs_charging_profiles: data_types::ChargingProfile {
                charging_profile_id: 0, // FIXME keep track of this
                transaction_id: None,
                stack_level: 0, // highest gets highest priority
                charging_profile_purpose: ChargingProfilePurposeType::ChargePointMaxProfile,
                // FIXME unsure whether this is supported by the charging point
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
                    start_schedule: Some(data_types::DateTimeWrapper::new(
                        state.start_schedule_utc,
                    )),
                    charging_rate_unit: ChargingRateUnitType::W,
                    charging_schedule_period: charging_schedule,
                    min_charging_rate: None,
                },
            },
        })
    }
}

#[derive(Debug)]
pub struct ChargingSchedulePeriodBuild {
    start: NaiveDateTime,
    end: NaiveDateTime,
    limit: f32,
}

impl ChargingSchedulePeriodBuild {
    #[expect(unused)]
    pub fn new(start: NaiveDateTime, end: NaiveDateTime) -> Result<Self, ()> {
        if end < start {
            return Err(());
        }
        Ok(ChargingSchedulePeriodBuild {
            limit: DEFAULT_LIMIT,
            start,
            end,
        })
    }

    #[expect(unused)]
    pub fn with_duration(start: NaiveDateTime, duration: TimeDelta) -> Self {
        ChargingSchedulePeriodBuild {
            limit: DEFAULT_LIMIT,
            start,
            end: start + duration,
        }
    }

    #[expect(unused)]
    pub fn starting_ending_today(start_time: NaiveTime, end_time: NaiveTime) -> Result<Self, ()> {
        if end_time < start_time {
            return Err(());
        }
        let today = Local::now().date_naive();
        Ok(ChargingSchedulePeriodBuild {
            limit: DEFAULT_LIMIT,
            start: NaiveDateTime::new(today, start_time),
            end: NaiveDateTime::new(today, end_time),
        })
    }

    #[expect(unused)]
    pub fn starting_today_with_duration(start_time: NaiveTime, duration: TimeDelta) -> Self {
        let today = Local::now().date_naive();
        let start = NaiveDateTime::new(today, start_time);
        ChargingSchedulePeriodBuild {
            limit: DEFAULT_LIMIT,
            start,
            end: start + duration,
        }
    }

    pub fn starting_ending_tomorrow(
        start_time: NaiveTime,
        end_time: NaiveTime,
    ) -> Result<Self, ()> {
        if end_time < start_time {
            return Err(());
        }
        let tomorrow = Local::now().date_naive() + TimeDelta::days(1);
        Ok(ChargingSchedulePeriodBuild {
            limit: DEFAULT_LIMIT,
            start: NaiveDateTime::new(tomorrow, start_time),
            end: NaiveDateTime::new(tomorrow, end_time),
        })
    }

    pub fn starting_tomorrow_with_duration(start_time: NaiveTime, duration: TimeDelta) -> Self {
        let tomorrow = Local::now().date_naive() + TimeDelta::days(1);
        let start = NaiveDateTime::new(tomorrow, start_time);
        ChargingSchedulePeriodBuild {
            limit: DEFAULT_LIMIT,
            start,
            end: start + duration,
        }
    }

    #[expect(unused)]
    pub fn starting_today_ending_tomorrow(start_time: NaiveTime, end_time: NaiveTime) -> Self {
        let today = Local::now().date_naive();
        let tomorrow = Local::now().date_naive() + TimeDelta::days(1);
        ChargingSchedulePeriodBuild {
            limit: DEFAULT_LIMIT,
            start: NaiveDateTime::new(today, start_time),
            end: NaiveDateTime::new(tomorrow, end_time),
        }
    }

    #[expect(unused)]
    pub fn limit(mut self, limit: f32) -> Self {
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

// FIXME add tests
