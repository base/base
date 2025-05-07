/// Set up the logger for the proposer.
pub fn setup_proposer_logger() {
    // Set up logging using the provided format
    let format = tracing_subscriber::fmt::format()
        .with_level(true)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(true);

    // Turn off all logging from kona and SP1.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into())
                .add_directive("single_hint_handler=error".parse().unwrap())
                .add_directive("execute=error".parse().unwrap())
                .add_directive("sp1_prover=error".parse().unwrap())
                .add_directive("boot_loader=error".parse().unwrap())
                .add_directive("client_executor=error".parse().unwrap())
                .add_directive("client=error".parse().unwrap())
                .add_directive("channel_assembler=error".parse().unwrap())
                .add_directive("attributes_queue=error".parse().unwrap())
                .add_directive("batch_validator=error".parse().unwrap())
                .add_directive("batch_queue=error".parse().unwrap())
                .add_directive("client_derivation_driver=error".parse().unwrap())
                .add_directive("block_builder=error".parse().unwrap())
                .add_directive("host_server=error".parse().unwrap())
                .add_directive("kona_protocol=error".parse().unwrap())
                .add_directive("sp1_core_executor=off".parse().unwrap())
                .add_directive("sp1_core_machine=error".parse().unwrap()),
        )
        .event_format(format)
        .init();
}
