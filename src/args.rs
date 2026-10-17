use anyhow::{Context, bail};
use chrono::NaiveTime;

use std::net::Ipv4Addr;

#[derive(Debug, clap::Parser)]
#[command(version, about = "Runs an OCPP server")]
#[command(propagate_version = true)]
pub struct Args {
    #[clap(long, help = "battery capacity (Wh)", default_value_t = 48_100)]
    pub battery_capacity: u32,

    #[clap(long, help = "charging plan power limit (W)", default_value_t = 7_400)]
    pub power_limit: u32,

    #[clap(
        long,
        help = "Constant power lost while charging (W)",
        default_value_t = 400
    )]
    pub constant_power_loss: u16,

    #[clap(
        long,
        help = "initial State of Charge (%) at start of a session (upcoming or in progress)"
    )]
    pub init: Option<u8>,

    #[clap(
        long,
        help = "State of Charge upper limit (%). Use more than 100 to discard previous SoC limit"
    )]
    pub target: Option<u8>,

    #[clap(
        long,
        help = "which charging plan to apply (default: no changes to current plan)"
    )]
    pub plan: Option<ChargingPlan>,

    #[clap(
        long,
        help = "Deadline Charging plan end time - required for reach-soc-cap-before ('HH:MM')"
    )]
    pub deadline: Option<String>,

    #[clap(long, help = "port of the OCPP server", default_value_t = 9000)]
    pub ocpp_port: u16,

    #[clap(
        long,
        help = "generate short logs: no timestamps, no modules. Suitable for a systemd unit"
    )]
    pub short_logs: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Args {
    /// Check Args consistency
    pub fn check(&self) -> anyhow::Result<()> {
        use Command::*;
        match &self.command {
            None
            | Some(ClearChargingProfile)
            | Some(GetConfiguration)
            | Some(Run)
            | Some(StopSession)
            | Some(Reboot)
            | Some(ResetMeterValueConfiguration)
            | Some(SetMeterValueConfiguration)
            | Some(SetMeterValueAlignedInterval(_)) => (),
            Some(SetServerIp(cmd_args)) => {
                let _ = cmd_args
                    .get_ip_address()
                    .context("checking set-server-ip command")?;
            }
        }

        Ok(())
    }
}

#[derive(clap::Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum Command {
    #[clap(
        about = "Clear the Charging Profile. Useful after switching fron another app, otherwise not needed"
    )]
    ClearChargingProfile,
    #[clap(about = "Get Charge Point Configuration")]
    GetConfiguration,
    #[clap(about = "Run the server")]
    Run,
    #[clap(about = "Stop the active session")]
    StopSession,
    #[clap(about = "Reboot the EVSE once connected. This will \
        replace any charging schedule with a permanent 0 W limit")]
    Reboot,
    #[clap(about = "Reset the MeterValue related keys to their default values")]
    ResetMeterValueConfiguration,
    #[clap(about = "Set the MeterValue related keys to values appropriate for my CP")]
    SetMeterValueConfiguration,
    #[clap(about = "Change the MeterValueSampleInterval key")]
    SetMeterValueAlignedInterval(Interval),
    #[clap(about = "Change the server IP, this will also reboot and \
        replace any charging schedule with a permanent 0 W limit")]
    SetServerIp(ServerIp),
}

impl Command {
    pub fn is_run(&self) -> bool {
        matches!(self, Command::Run)
    }
    pub fn is_stop_session(&self) -> bool {
        matches!(self, Command::StopSession)
    }
    pub fn is_reboot(&self) -> bool {
        matches!(self, Command::Reboot)
    }
}

impl Args {
    pub fn is_dry_run(&self) -> bool {
        self.command.is_none()
    }
    pub fn is_run(&self) -> bool {
        self.command.as_ref().is_some_and(|c| c.is_run())
    }
    pub fn is_stop_session(&self) -> bool {
        self.command.as_ref().is_some_and(|c| c.is_stop_session())
    }
    pub fn is_reboot(&self) -> bool {
        self.command.as_ref().is_some_and(|c| c.is_reboot())
    }
}

impl From<&Args> for crate::Bms {
    fn from(args: &Args) -> Self {
        use crate::SoC;
        crate::Bms::builder(args.battery_capacity, args.constant_power_loss)
            .initial_soc(
                // FIXME also support relative at some point
                args.init
                    .map_or(SoC::Unknown, |soc| SoC::Absolute(soc as f64 / 100.0)),
            )
            .target_soc(args.target.map(|soc| soc as f64 / 100.0))
            .build()
    }
}

#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum ChargingPlan {
    #[clap(help = "Block charging (0 W permanent plan)")]
    Blocking,
    #[clap(help = "Off peak period Today")]
    OffPeakToday,
    #[clap(help = "Off peak period Tomorrow")]
    OffPeakTomorrow,
    #[clap(help = "Compute a start time so the battery reaches SoC target \
        as close as possible to stop time (needs to specify SoC target & deadline)")]
    Deadline,
    #[clap(help = "Start ASAP")]
    NoLimit,
}

impl TryFrom<&Args> for Option<crate::ChargingPlan> {
    // FIXME
    type Error = anyhow::Error;
    fn try_from(args: &Args) -> anyhow::Result<Self> {
        let Some(charging_plan) = args.plan else {
            return Ok(None);
        };

        match charging_plan {
            ChargingPlan::Blocking => Ok(Some(crate::ChargingPlan::Blocking)),
            ChargingPlan::OffPeakToday => Ok(Some(crate::ChargingPlan::OffPeakToday {
                power_limit: args.power_limit,
            })),
            ChargingPlan::OffPeakTomorrow => Ok(Some(crate::ChargingPlan::OffPeakTomorrow {
                power_limit: args.power_limit,
            })),
            ChargingPlan::Deadline => {
                let mut err_msg = String::new();
                if args.target.is_none() {
                    err_msg.push_str("'deadline' plan requires SoC 'target' to be specified");
                }
                if let Some(ref end_time) = args.deadline {
                    match end_time.parse::<NaiveTime>() {
                        Ok(end_time) => {
                            if err_msg.is_empty() {
                                return Ok(Some(crate::ChargingPlan::Deadline {
                                    deadline: end_time,
                                    power_limit: args.power_limit,
                                }));
                            }
                        }
                        Err(err) => {
                            if !err_msg.is_empty() {
                                err_msg.push('\n');
                            }
                            err_msg.push_str("'deadline' plan failed to parse 'deadline' time: ");
                            err_msg.push_str(&err.to_string());
                        }
                    }
                } else {
                    if !err_msg.is_empty() {
                        err_msg.push('\n');
                    }
                    err_msg.push_str("'deadline' plan requires 'deadline' time to be specified");
                }

                bail!(err_msg);
            }
            ChargingPlan::NoLimit => Ok(Some(crate::ChargingPlan::NoLimit)),
        }
    }
}

#[derive(clap::Parser, Clone, Debug, Default, PartialEq, Eq)]
#[clap(about = "interval in seconds")]
pub struct Interval {
    #[clap(long, help = "interval in seconds", default_value_t = 60)]
    pub interval: u16,
}

#[derive(clap::Parser, Clone, Debug, Default, PartialEq, Eq)]
#[clap(about = "server IP address")]
pub struct ServerIp {
    #[clap(
        long,
        help = "server IP address (used by command set-server-ip)",
        default_value = "192.168.1.49"
    )]
    pub ip_address: String,
}

impl ServerIp {
    // FIXME use proper error type
    pub fn get_ip_address(&self) -> anyhow::Result<Ipv4Addr> {
        self.ip_address
            .parse::<Ipv4Addr>()
            .context("server IP address")
    }
}
