pub fn init() {
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
