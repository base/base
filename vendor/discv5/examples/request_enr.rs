//! Creates a Discv5 Server and requests an ENR of a node given a multiaddr.
//!
//! This is a simple example of how one may connect to a Discv5 peer using a multiaddr/multiaddr
//! string.
//!
//! To run this example execute the following command from the root directory:
//! ```bash
//! $ cargo run --example request_enr <MULTIADDR>
//! ```
//!
//! The <MULTIADDR> value should be the string form of a multiaddr including the p2p protocol.
//! Currently only secp256k1 and ed25519 keys are supported.
//!
//! This requires the "libp2p" feature.
#[cfg(feature = "libp2p")]
use std::net::Ipv4Addr;

#[cfg(feature = "libp2p")]
use discv5::ConfigBuilder;
#[cfg(feature = "libp2p")]
use discv5::ListenConfig;
#[cfg(feature = "libp2p")]
use discv5::{Discv5, enr::CombinedKey};

#[cfg(not(feature = "libp2p"))]
fn main() {}

#[cfg(feature = "libp2p")]
#[tokio::main]
async fn main() {
    let filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap();
    let _ = tracing_subscriber::fmt().with_env_filter(filter_layer).try_init();

    // listening address and port
    let listen_config = ListenConfig::Ipv4 { ip: Ipv4Addr::UNSPECIFIED, port: 9000 };

    // generate a new enr key
    let enr_key = CombinedKey::generate_secp256k1();
    // construct a local ENR
    let enr = enr::Enr::empty(&enr_key).unwrap();

    // default discv5 configuration
    let config = ConfigBuilder::new(listen_config).build();

    let multiaddr = std::env::args().nth(1).expect("A multiaddr must be supplied");

    // construct the discv5 server
    let mut discv5: Discv5 = Discv5::new(enr, enr_key, config).unwrap();

    // start the discv5 service
    discv5.start().await.unwrap();

    // search for the ENR
    match discv5.request_enr(multiaddr).await {
        Ok(enr) => {
            println!("ENR Found:");
            println!("Base64:{}", enr.to_base64());
            println!("{enr}");
        }
        Err(e) => {
            println!("Error:{e:?}");
        }
    }
}
