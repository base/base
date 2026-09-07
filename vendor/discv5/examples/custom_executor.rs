//! Demonstrates how to run a basic Discovery v5 Service with a custom tokio executor.
//!
//! Discv5 requires a Tokio executor with all features. A custom executor can be passed via the
//! configuration parameters. If none is passed, it will use the current runtime that build the
//! `Discv5` struct.
//!
//! To run this example simply run:
//! ```
//! $ cargo run --example custom_executor <BASE64ENR>
//! ```

use std::net::Ipv4Addr;

use discv5::{ConfigBuilder, Discv5, Event, ListenConfig, enr, enr::CombinedKey};

fn main() {
    // allows detailed logging with the RUST_LOG env variable
    let filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap();
    let _ = tracing_subscriber::fmt().with_env_filter(filter_layer).try_init();

    // listening address and port
    let listen_config = ListenConfig::Ipv4 { ip: Ipv4Addr::UNSPECIFIED, port: 9000 };

    let enr_key = CombinedKey::generate_secp256k1();
    // construct a local ENR
    let enr = enr::Enr::empty(&enr_key).unwrap();

    // build the tokio executor
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("Discv5-example")
        .enable_all()
        .build()
        .unwrap();

    // default configuration - uses the current executor
    let config = ConfigBuilder::new(listen_config).build();

    // construct the discv5 server
    let mut discv5: Discv5 = Discv5::new(enr, enr_key, config).unwrap();

    // if we know of another peer's ENR, add it known peers
    if let Some(base64_enr) = std::env::args().nth(1) {
        match base64_enr.parse::<enr::Enr<CombinedKey>>() {
            Ok(enr) => {
                println!(
                    "ENR Read. ip: {:?}, udp_port {:?}, tcp_port: {:?}",
                    enr.ip4(),
                    enr.udp4(),
                    enr.tcp4()
                );
                if let Err(e) = discv5.add_enr(enr) {
                    println!("ENR was not added: {e}");
                }
            }
            Err(e) => panic!("Decoding ENR failed: {}", e),
        }
    }

    runtime.block_on(async {
        // start the discv5 service
        discv5.start().await.unwrap();
        println!("Server started");

        // get an event stream
        let mut event_stream = discv5.event_stream().await.unwrap();

        loop {
            match event_stream.recv().await {
                Some(Event::SocketUpdated(addr)) => {
                    println!("Nodes ENR socket address has been updated to: {addr:?}");
                }
                Some(Event::Discovered(enr)) => {
                    println!("A peer has been discovered: {}", enr.node_id());
                }
                _ => {}
            }
        }
    })
}
