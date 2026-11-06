pub fn init() {
    use std::sync::Once;
    static LOGGER: Once = Once::new();
    LOGGER.call_once(|| {
        use tracing_subscriber::{filter::LevelFilter, prelude::*};

        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_thread_ids(true)
                    .with_line_number(true),
            )
            .with(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(LevelFilter::ERROR.into())
                    .from_env_lossy(),
            )
            .init();
    });
}
