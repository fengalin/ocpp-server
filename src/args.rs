use anyhow::bail;
use chrono::NaiveTime;

#[derive(Debug, clap::Parser)]
#[command(version, about = "Runs an OCPP server")]
#[command(propagate_version = true)]
pub struct Args {
    #[clap(long, help = "port of the OCPP server", default_value_t = 9000)]
    pub ocpp_port: u16,

    #[clap(
        long,
        help = "actually run the server, \
        otherwise just performs checks & print initial configuration"
    )]
    pub run: bool,

    #[clap(long, help = "battery capacity (Wh)", default_value_t = 48_100)]
    pub battery_capacity: u32,

    #[clap(
        long,
        help = "Constant power lost while charging (W)",
        default_value_t = 400
    )]
    pub constant_power_loss: u16,

    #[clap(long, help = "initial State of Charge (%)")]
    pub initial_soc: Option<u8>,

    #[clap(long, help = "State of Charge upper limit (%)")]
    pub soc_cap: Option<u8>,

    #[clap(
        long,
        help = "which charging plan to apply (default: no changes to current plan)"
    )]
    pub charging_plan: Option<ChargingPlan>,

    #[clap(
        long,
        help = "Charging plan end time - required for reach-soc-cap-before ('HH:MM')"
    )]
    pub end_time: Option<String>,
}

impl From<&Args> for crate::Bms {
    fn from(args: &Args) -> Self {
        // FIXME check initial / cap SoC consistency + range
        crate::Bms {
            capacity: args.battery_capacity as f64,
            constant_power_loss: args.constant_power_loss,
            initial_soc: args.initial_soc.map(|soc| soc as f64 / 100.0),
            soc_cap: args.soc_cap.map(|soc| soc as f64 / 100.0),
        }
    }
}

#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum ChargingPlan {
    #[clap(help = "Off peak period Today")]
    OffPeakToday,
    #[clap(help = "Off peak period Tomorrow")]
    OffPeakTomorrow,
    #[clap(help = "Compute a start time so the battery reaches SoC cap \
        as close as possible to stop time (needs to specify soc_cap & stop_time)")]
    ReachSocCapBefore,
    #[clap(help = "Start ASAP")]
    NoLimit,
}

impl TryFrom<&Args> for Option<crate::ChargingPlan> {
    // FIXME
    type Error = anyhow::Error;
    fn try_from(args: &Args) -> anyhow::Result<Self> {
        let Some(charging_plan) = args.charging_plan else {
            return Ok(None);
        };

        match charging_plan {
            ChargingPlan::OffPeakToday => Ok(Some(crate::ChargingPlan::OffPeakToday)),
            ChargingPlan::OffPeakTomorrow => Ok(Some(crate::ChargingPlan::OffPeakTomorrow)),
            ChargingPlan::ReachSocCapBefore => {
                let mut err_msg = String::new();
                if args.soc_cap.is_none() {
                    err_msg.push_str("'reach-soc-cap-before' requires soc-cap to be specified");
                }
                if let Some(ref end_time) = args.end_time {
                    match end_time.parse::<NaiveTime>() {
                        Ok(end_time) => {
                            return Ok(Some(crate::ChargingPlan::ReachSocCapBefore { end_time }));
                        }
                        Err(err) => {
                            if !err_msg.is_empty() {
                                err_msg.push('\n');
                            }
                            err_msg.push_str("'reach-soc-cap-before' failed to parse end_time: ");
                            err_msg.push_str(&err.to_string());
                        }
                    }
                } else {
                    if !err_msg.is_empty() {
                        err_msg.push('\n');
                    }
                    err_msg.push_str("'reach-soc-cap-before' requires soc-cap to be specified");
                }

                bail!(err_msg);
            }
            ChargingPlan::NoLimit => Ok(Some(crate::ChargingPlan::NoLimit)),
        }
    }
}
