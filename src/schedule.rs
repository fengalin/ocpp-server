use anyhow::{Context, bail};
use chrono::{Local, NaiveDateTime, Utc};
use ocpp_rs::{
    datetime::DateTimeWrapper,
    v16::{call, data_types, enums::*},
};

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

    let now = Utc::now();

    // FIXME allow setting start in the past
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
