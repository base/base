//! Demonstrates how to run a basic Discovery v5 Service.
//!
//! This example simply starts a discovery server and listens to events that the server emits.
//!
//!
//! It can be bootstrapped to a DHT by providing an ENR to add to its DHT.
//!
//! To run this example simply run:
//! ```
//! $ cargo run --example simple_server -- <ENR-IP> <ENR-PORT> <BASE64ENR>
//! ```

use std::net::Ipv4Addr;

use discv5::{ConfigBuilder, Discv5, Event, ListenConfig, enr, enr::CombinedKey};

#[tokio::main]
async fn main() {
    // allows detailed logging with the RUST_LOG env variable
    let filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap();
    let _ = tracing_subscriber::fmt().with_env_filter(filter_layer).try_init();

    // if there is an address specified use it
    let address = std::env::args().nth(1).map(|addr| addr.parse::<Ipv4Addr>().unwrap());

    let port = {
        if let Some(udp_port) = std::env::args().nth(2) { udp_port.parse().unwrap() } else { 9000 }
    };

    // listening address and port
    let listen_config = ListenConfig::Ipv4 { ip: Ipv4Addr::UNSPECIFIED, port: 9000 };

    let enr_key = CombinedKey::generate_secp256k1();

    // construct a local ENR
    let enr = {
        let mut builder = enr::Enr::builder();
        // if an IP was specified, use it
        if let Some(external_address) = address {
            builder.ip4(external_address);
        }
        // if a port was specified, use it
        if std::env::args().nth(2).is_some() {
            builder.udp4(port);
        }
        builder.build(&enr_key).unwrap()
    };

    // if the ENR is useful print it
    println!("Node Id: {}", enr.node_id());
    if enr.udp4_socket().is_some() {
        println!("Base64 ENR: {}", enr.to_base64());
        println!("IP: {}, UDP_PORT:{}", enr.ip4().unwrap(), enr.udp4().unwrap());
    } else {
        println!("ENR is not printed as no IP:PORT was specified");
    }

    // default configuration
    let config = ConfigBuilder::new(listen_config).build();

    // construct the discv5 server
    let mut discv5: Discv5 = Discv5::new(enr, enr_key, config).unwrap();

    // if we know of another peer's ENR, add it known peers
    if let Some(base64_enr) = std::env::args().nth(3) {
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
}
