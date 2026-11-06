use charge_system_common_model::bms::Bms;

#[derive(Debug, clap::Parser)]
#[command(version, about = "Runs an OCPP server")]
#[command(propagate_version = true)]
pub struct Args {
    #[clap(long, help = "battery capacity (Wh)", default_value_t = 48_100)]
    pub battery_capacity: u32,

    #[clap(long, help = "charging plan power limit (W)", default_value_t = 7_400)]
    pub power_limit: u32,

    #[clap(long, help = "port of the OCPP server", default_value_t = 9000)]
    pub ocpp_port: u16,

    #[clap(
        long,
        help = "drop the CP websocket connection when no message arrives within \
                this many seconds (the CP pings every 40 s, so this only fires on \
                a dead connection)",
        default_value_t = 60
    )]
    pub ocpp_read_timeout: u16,

    #[clap(long, help = "port of the web server", default_value_t = 9080)]
    pub webserver_port: u16,

    #[clap(
        long,
        help = "generate short logs: no timestamps, no modules. Suitable for a systemd unit"
    )]
    pub short_logs: bool,
}

impl From<&Args> for Bms {
    fn from(args: &Args) -> Self {
        Bms::builder(args.battery_capacity)
            .build()
            .expect("can't fail as is")
    }
}
