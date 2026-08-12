#[derive(Debug, clap::Parser)]
#[command(version, about = "Runs an OCPP server")]
#[command(propagate_version = true)]
pub struct Args {
    #[clap(long, help = "port of the OCPP server", default_value_t = 9000)]
    pub ocpp_port: u16,

    #[clap(long, help = "battery capacity (Wh)", default_value_t = 48_100)]
    pub battery_capacity: u32,

    #[clap(long, help = "initial State of Charge (%)")]
    pub initial_soc: Option<u8>,

    #[clap(long, help = "State of Charge limit (%)")]
    pub soc_limit: Option<u8>,

    #[clap(
        long,
        help = "which charging plan to apply (default: no changes to current plan)"
    )]
    pub charging_plan: Option<ChargingPlan>,
}

#[derive(Debug, Copy, Clone)]
pub struct Battery {
    pub capacity: f64,
    pub initial_soc: Option<f64>,
    pub soc_limit: Option<f64>,
}

impl From<&Args> for Battery {
    fn from(args: &Args) -> Self {
        Battery {
            capacity: args.battery_capacity as f64,
            initial_soc: args.initial_soc.map(|soc| soc as f64 / 100.0),
            soc_limit: args.soc_limit.map(|soc| soc as f64 / 100.0),
        }
    }
}

#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum ChargingPlan {
    #[clap(help = "Off peak period Today")]
    OffPeakPeriodToday,
    #[clap(help = "Off peak period Tomorrow")]
    OffPeakPeriodTomorrow,
    #[clap(help = "Start ASAP")]
    NoLimit,
}
