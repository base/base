#![cfg(test)]

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr},
};

use alloy_rlp::bytes::Bytes;
use enr::{CombinedKey, Enr, EnrKey, NodeId, k256};
use rand_core::{RngCore, SeedableRng};

use crate::{Discv5, socket::ListenConfig, *};

fn init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

fn update_enr<T: alloy_rlp::Encodable>(discv5: &mut Discv5, key: &str, value: &T) -> bool {
    discv5.enr_insert(key, value).is_ok()
}

#[allow(dead_code)]
async fn build_nodes(n: usize, base_port: u16) -> Vec<Discv5> {
    let mut nodes = Vec::new();
    let ip: Ipv4Addr = "127.0.0.1".parse().unwrap();

    for port in base_port..base_port + n as u16 {
        let enr_key = CombinedKey::generate_secp256k1();
        let listen_config = ListenConfig::Ipv4 { ip, port };
        let config = ConfigBuilder::new(listen_config).build();

        let enr = Enr::builder().ip4(ip).udp4(port).build(&enr_key).unwrap();
        // transport for building a swarm
        let mut discv5 = Discv5::new(enr, enr_key, config).unwrap();
        discv5.start().await.unwrap();
        nodes.push(discv5);
    }
    nodes
}

/// Build `n` swarms using passed keypairs.
async fn build_nodes_from_keypairs(keys: Vec<CombinedKey>, base_port: u16) -> Vec<Discv5> {
    let mut nodes = Vec::new();
    let ip: Ipv4Addr = "127.0.0.1".parse().unwrap();

    for (i, enr_key) in keys.into_iter().enumerate() {
        let port = base_port + i as u16;

        let listen_config = ListenConfig::Ipv4 { ip, port };
        let config = ConfigBuilder::new(listen_config).build();

        let enr = Enr::builder().ip4(ip).udp4(port).build(&enr_key).unwrap();

        let mut discv5 = Discv5::new(enr, enr_key, config).unwrap();
        discv5.start().await.unwrap();
        nodes.push(discv5);
    }
    nodes
}

async fn build_nodes_from_keypairs_ipv6(keys: Vec<CombinedKey>, base_port: u16) -> Vec<Discv5> {
    let mut nodes = Vec::new();

    for (i, enr_key) in keys.into_iter().enumerate() {
        let port = base_port + i as u16;

        let listen_config = ListenConfig::Ipv6 { ip: Ipv6Addr::LOCALHOST, port };
        let config = ConfigBuilder::new(listen_config).build();

        let enr = Enr::builder().ip6(Ipv6Addr::LOCALHOST).udp6(port).build(&enr_key).unwrap();

        let mut discv5 = Discv5::new(enr, enr_key, config).unwrap();
        discv5.start().await.unwrap();
        nodes.push(discv5);
    }
    nodes
}

async fn build_nodes_from_keypairs_dual_stack(
    keys: Vec<CombinedKey>,
    base_port: u16,
) -> Vec<Discv5> {
    let mut nodes = Vec::new();

    for (i, enr_key) in keys.into_iter().enumerate() {
        let ipv4_port = base_port + i as u16;
        let ipv6_port = ipv4_port + 1000;

        let listen_config = ListenConfig::DualStack {
            ipv4: Ipv4Addr::LOCALHOST,
            ipv4_port,
            ipv6: Ipv6Addr::LOCALHOST,
            ipv6_port,
        };
        let config = ConfigBuilder::new(listen_config).build();

        let enr = Enr::builder()
            .ip4(Ipv4Addr::LOCALHOST)
            .udp4(ipv4_port)
            .ip6(Ipv6Addr::LOCALHOST)
            .udp6(ipv6_port)
            .build(&enr_key)
            .unwrap();

        let mut discv5 = Discv5::new(enr, enr_key, config).unwrap();
        discv5.start().await.unwrap();
        nodes.push(discv5);
    }
    nodes
}

/// Generate `n` deterministic keypairs from a given seed.
pub(crate) fn generate_deterministic_keypair(n: usize, seed: u64) -> Vec<CombinedKey> {
    let mut keypairs = Vec::new();
    for i in 0..n {
        let sk = {
            let rng = &mut rand_xorshift::XorShiftRng::seed_from_u64(seed + i as u64);
            let mut b = [0; 32];
            loop {
                // until a value is given within the curve order
                rng.fill_bytes(&mut b);
                if let Ok(k) = k256::ecdsa::SigningKey::from_slice(&b) {
                    break k;
                }
            }
        };
        let kp = CombinedKey::from(sk);
        keypairs.push(kp);
    }
    keypairs
}

fn get_distance(node1: NodeId, node2: NodeId) -> Option<u64> {
    let node1: Key<NodeId> = node1.into();
    node1.log2_distance(&node2.into())
}

#[macro_export]
macro_rules! return_if_ipv6_is_not_supported {
    () => {
        let mut is_ipv6_supported = false;
        for i in if_addrs::get_if_addrs().expect("network interfaces").iter() {
            if !i.is_loopback() && i.addr.ip().is_ipv6() {
                is_ipv6_supported = true;
                break;
            }
        }

        if !is_ipv6_supported {
            tracing::error!("Seems Ipv6 is not supported. Test won't be run.");
            return;
        }
    };
}

// Simple searching function to find seeds that give node ids for a range of testing and different
// topologies
#[allow(dead_code)]
fn find_seed_same_bucket() {
    let mut seed = 1;
    'main: loop {
        if seed % 1000 == 0 {
            println!("Seed: {seed}");
        }

        let keys = generate_deterministic_keypair(11, seed);

        let node_ids = keys.into_iter().map(|k| NodeId::from(k.public())).collect::<Vec<_>>();

        let local = node_ids[0];

        for &id in node_ids[1..].iter() {
            let distance = get_distance(local, id);
            if distance != Some(256) {
                seed += 1;
                continue 'main;
            }
        }
        break;
    }
    println!("Found Seed: {seed}");
}

#[allow(dead_code)]
fn find_seed_spread_bucket() {
    let mut buckets;
    let mut seed = 1;
    loop {
        seed += 1;

        let keys = generate_deterministic_keypair(11, seed);

        let node_ids = keys.into_iter().map(|k| NodeId::from(k.public())).collect::<Vec<_>>();

        let local = node_ids[0];

        buckets = HashMap::new();

        for &id in node_ids[1..].iter() {
            let distance = get_distance(local, id);
            if let Some(distance) = distance {
                *buckets.entry(distance).or_insert_with(|| 0) += 1;
            }
        }
        if !buckets.values().any(|v| *v > 2) {
            break;
        }
        if seed % 1000 == 0 {
            println!("Seed: {seed}");
        }
    }
    println!("Found Seed: {seed}");
    for (k, v) in buckets.iter() {
        println!("{k}, {v}");
    }
}

/// Find a seed that gives nodes in a linear topology for query searching.
///
/// The target and the next node are within the last few buckets.
///
/// So we can do:
/// N1 -> N2 -> N3 -> ..... NX
/// in a query when searching for target. This means target an N+1 are in N's last few buckets.
#[allow(dead_code)]
fn find_seed_linear_topology() {
    let mut seed = 1;
    let bucket_tolerance = 3; // Target and next node must be in the last `bucket_tolerance` buckets.
    let mut main_result;
    let ordering;
    'main: loop {
        seed += 1;
        if seed % 1000 == 0 {
            println!("Trying seed: {seed}");
        }

        let keys = generate_deterministic_keypair(11, seed);

        let orig_node_ids = keys.into_iter().map(|k| NodeId::from(k.public())).collect::<Vec<_>>();

        let mut node_ids = orig_node_ids.clone();

        let target = node_ids.remove(0);

        let mut result = Vec::new();

        // Can we arrange the rest of the nodes in some linear way.
        while !node_ids.is_empty() {
            let id = node_ids.remove(0);

            let distance = get_distance(target, id).unwrap_or(0);
            // The target must be in the first bucket_tolerance buckets.
            if distance <= 256 - bucket_tolerance {
                continue 'main;
            }

            // If this is the first node, add it to the result list and continue
            if result.is_empty() {
                result.push(id);
            } else if !node_ids.is_empty() {
                // try and find a linear match
                match node_ids
                    .iter()
                    .position(|id| get_distance(target, *id).unwrap_or(0) >= 256 - bucket_tolerance)
                {
                    Some(pos) => {
                        let matching_id = node_ids.remove(pos);
                        if get_distance(target, matching_id).unwrap_or(0) < 256 - bucket_tolerance {
                            continue 'main; // all nodes need to be in this distance
                        }
                        result.push(id);
                        result.push(matching_id);
                    }
                    None => {
                        continue 'main;
                    }
                }
            } else {
                result.push(id);
            }
        }
        main_result = result;
        // Target sits at the start
        main_result.insert(0, target);
        ordering = main_result
            .iter()
            .map(|id| orig_node_ids.iter().position(|x| x == id).unwrap())
            .collect::<Vec<_>>();
        break;
    }
    // We've found a solution. Check it.
    println!("Found Seed: {seed}");
    println!("Ordering: {ordering:?}");
    let target = main_result.remove(0);
    // remove the target
    println!("Target: {target}");
    for (x, id) in main_result.iter().enumerate() {
        println!("Node{x}: {id}");
    }

    for (node, previous_node) in main_result.iter().skip(1).zip(main_result.clone()) {
        let key: Key<NodeId> = (*node).into();
        let distance = key.log2_distance(&previous_node.into()).unwrap();
        let target_distance = key.log2_distance(&target.into()).unwrap();
        println!(
            "Distance of node {previous_node} relative to next node: {node} is: {distance},  relative to target {target_distance}"
        );
    }
}

/// Test for running a simple query test for a topology consisting of IPv4 nodes.
#[tokio::test]
async fn test_discovery_three_peers_ipv4() {
    init();
    let total_nodes = 3;
    // Seed is chosen such that all nodes are in the 256th bucket of bootstrap
    let seed = 1652;
    // Generate `num_nodes` + bootstrap_node and target_node keypairs from given seed
    let keypairs = generate_deterministic_keypair(total_nodes + 2, seed);
    // IPv4
    let nodes = build_nodes_from_keypairs(keypairs, 10000).await;

    assert_eq!(total_nodes, test_discovery_three_peers(nodes, total_nodes).await);
}

/// Test for running a simple query test for a topology consisting of IPv6 nodes.
#[tokio::test]
async fn test_discovery_three_peers_ipv6() {
    return_if_ipv6_is_not_supported!();

    init();
    let total_nodes = 3;
    // Seed is chosen such that all nodes are in the 256th bucket of bootstrap
    let seed = 1652;
    // Generate `num_nodes` + bootstrap_node and target_node keypairs from given seed
    let keypairs = generate_deterministic_keypair(total_nodes + 2, seed);
    // IPv6
    let nodes = build_nodes_from_keypairs_ipv6(keypairs, 10010).await;

    assert_eq!(total_nodes, test_discovery_three_peers(nodes, total_nodes).await);
}

/// Test for running a simple query test for a topology consisting of dual stack nodes.
#[tokio::test]
async fn test_discovery_three_peers_dual_stack() {
    return_if_ipv6_is_not_supported!();

    init();
    let total_nodes = 3;
    // Seed is chosen such that all nodes are in the 256th bucket of bootstrap
    let seed = 1652;
    // Generate `num_nodes` + bootstrap_node and target_node keypairs from given seed
    let keypairs = generate_deterministic_keypair(total_nodes + 2, seed);
    // DualStack
    let nodes = build_nodes_from_keypairs_dual_stack(keypairs, 10020).await;

    assert_eq!(total_nodes, test_discovery_three_peers(nodes, total_nodes).await);
}

/// Test for running a simple query test for a mixed topology of IPv4, IPv6 and dual stack nodes.
/// The node to run the query is DualStack.
#[tokio::test]
async fn test_discovery_three_peers_mixed() {
    return_if_ipv6_is_not_supported!();

    init();
    let total_nodes = 3;
    // Seed is chosen such that all nodes are in the 256th bucket of bootstrap
    let seed = 1652;
    // Generate `num_nodes` + bootstrap_node and target_node keypairs from given seed
    let mut keypairs = generate_deterministic_keypair(total_nodes + 2, seed);

    let mut nodes = vec![];
    // Bootstrap node (DualStack)
    nodes.append(&mut build_nodes_from_keypairs_dual_stack(vec![keypairs.remove(0)], 10030).await);
    // A node to run query (DualStack)
    nodes.append(&mut build_nodes_from_keypairs_dual_stack(vec![keypairs.remove(0)], 10031).await);
    // IPv4 node
    nodes.append(&mut build_nodes_from_keypairs(vec![keypairs.remove(0)], 10032).await);
    // IPv6 node
    nodes.append(&mut build_nodes_from_keypairs_ipv6(vec![keypairs.remove(0)], 10033).await);
    // Target node (DualStack)
    nodes.append(&mut build_nodes_from_keypairs_dual_stack(vec![keypairs.remove(0)], 10034).await);

    assert!(keypairs.is_empty());
    assert_eq!(5, nodes.len());
    assert_eq!(total_nodes, test_discovery_three_peers(nodes, total_nodes).await);
}

/// Test for running a simple query test for a mixed topology of IPv4, IPv6 and dual stack nodes.
/// The node to run the query is IPv4.
// NOTE: This test emits the error log below because the node to run a query is in IPv4 mode so
// IPv6 address included in the response is non-contactable.
// `ERROR discv5::service: Query 0 has a non contactable enr: ENR: NodeId: 0xe030..dcbe, IpV4 Socket: None IpV6 Socket: Some([::1]:10043)`
#[tokio::test]
async fn test_discovery_three_peers_mixed_query_from_ipv4() {
    return_if_ipv6_is_not_supported!();

    init();
    let total_nodes = 3;
    // Seed is chosen such that all nodes are in the 256th bucket of bootstrap
    let seed = 1652;
    // Generate `num_nodes` + bootstrap_node and target_node keypairs from given seed
    let mut keypairs = generate_deterministic_keypair(total_nodes + 2, seed);

    let mut nodes = vec![];
    // Bootstrap node (DualStack)
    nodes.append(&mut build_nodes_from_keypairs_dual_stack(vec![keypairs.remove(0)], 10040).await);
    // A node to run query (** IPv4 **)
    nodes.append(&mut build_nodes_from_keypairs(vec![keypairs.remove(0)], 10041).await);
    // IPv4 node
    nodes.append(&mut build_nodes_from_keypairs(vec![keypairs.remove(0)], 10042).await);
    // IPv6 node
    nodes.append(&mut build_nodes_from_keypairs_ipv6(vec![keypairs.remove(0)], 10043).await);
    // Target node (DualStack)
    nodes.append(&mut build_nodes_from_keypairs_dual_stack(vec![keypairs.remove(0)], 10044).await);

    assert!(keypairs.is_empty());
    assert_eq!(5, nodes.len());

    // `2` is expected here since the node that runs the query is IPv4.
    // The response from Bootstrap node will include the IPv6 node but that will be ignored due to
    // non-contactable.
    assert_eq!(2, test_discovery_three_peers(nodes, total_nodes).await);
}

/// This is a smaller version of the star topology test designed to debug issues with queries.
async fn test_discovery_three_peers(mut nodes: Vec<Discv5>, total_nodes: usize) -> usize {
    init();
    // Last node is bootstrap node in a star topology
    let bootstrap_node = nodes.remove(0);
    // target_node is not polled.
    let target_node = nodes.pop().unwrap();
    println!("Bootstrap node: {}", bootstrap_node.local_enr().node_id());
    println!("Target node: {}", target_node.local_enr().node_id());
    let key: Key<NodeId> = target_node.local_enr().node_id().into();
    let distance = key.log2_distance(&bootstrap_node.local_enr().node_id().into()).unwrap();
    println!(
        "Distance of target_node {} relative to bootstrap {}: {}",
        target_node.local_enr().node_id(),
        bootstrap_node.local_enr().node_id(),
        distance
    );
    for node in nodes.iter_mut() {
        let key: Key<NodeId> = node.local_enr().node_id().into();
        let distance = key.log2_distance(&bootstrap_node.local_enr().node_id().into()).unwrap();
        println!(
            "Distance of node {} relative to bootstrap {}: {}",
            node.local_enr().node_id(),
            bootstrap_node.local_enr().node_id(),
            distance
        );
        node.add_enr(bootstrap_node.local_enr().clone()).unwrap();
        bootstrap_node.add_enr(node.local_enr().clone()).unwrap();
    }

    // Start a FINDNODE query of target
    let target_random_node_id = target_node.local_enr().node_id();
    nodes.push(bootstrap_node);
    let result_nodes = nodes.first_mut().unwrap().find_node(target_random_node_id).await.unwrap();
    println!("Query found {} peers, Total peers {}", result_nodes.len(), total_nodes);
    result_nodes.len()
}

/// Test for a star topology with `num_nodes` connected to a `bootstrap_node`
/// FINDNODE request is sent from any of the `num_nodes` nodes to a `target_node`
/// which isn't part of the swarm.
/// The seed for the keypair generation is chosen such that all `num_nodes` nodes
/// and the `target_node` are in the 256th k-bucket of the bootstrap node.
/// This ensures that all nodes are found in a single FINDNODE query.
#[tokio::test]
async fn test_discovery_star_topology() {
    init();
    let total_nodes = 10;
    // Seed is chosen such that all nodes are in the 256th bucket of bootstrap
    let seed = 1652;
    // Generate `num_nodes` + bootstrap_node and target_node keypairs from given seed
    let keypairs = generate_deterministic_keypair(total_nodes + 2, seed);
    let mut nodes = build_nodes_from_keypairs(keypairs, 11000).await;
    // Last node is bootstrap node in a star topology
    let bootstrap_node = nodes.remove(0);
    // target_node is not polled.
    let target_node = nodes.pop().unwrap();
    println!("Bootstrap node: {}", bootstrap_node.local_enr().node_id());
    let key: Key<NodeId> = target_node.local_enr().node_id().into();
    let distance = key.log2_distance(&bootstrap_node.local_enr().node_id().into()).unwrap();
    println!("Target node: {}", target_node.local_enr().node_id());
    println!(
        "Distance of target_node {} relative to bootstrap {}: {}",
        target_node.local_enr().node_id(),
        bootstrap_node.local_enr().node_id(),
        distance
    );
    for node in nodes.iter_mut() {
        let key: Key<NodeId> = node.local_enr().node_id().into();
        let distance = key.log2_distance(&bootstrap_node.local_enr().node_id().into()).unwrap();
        println!(
            "Distance of node {} relative to bootstrap node {}: {}",
            node.local_enr().node_id(),
            bootstrap_node.local_enr().node_id(),
            distance
        );
        node.add_enr(bootstrap_node.local_enr().clone()).unwrap();
        bootstrap_node.add_enr(node.local_enr().clone()).unwrap();
    }
    // Start a FINDNODE query of target
    let target_random_node_id = target_node.local_enr().node_id();
    nodes.push(bootstrap_node);
    let result_nodes = nodes.first_mut().unwrap().find_node(target_random_node_id).await.unwrap();
    println!("Query found {} peers, Total peers {}", result_nodes.len(), total_nodes);
    assert_eq!(result_nodes.len(), total_nodes);
}

#[tokio::test]
async fn test_findnode_query() {
    init();
    // build a collection of 8 nodes
    let total_nodes = 8;
    // Seed is chosen for a linear topology. Each node is connected to each other and in the top 3
    // buckets from each other and the target.
    let mut keypairs = generate_deterministic_keypair(total_nodes + 1, 5);
    let target_node_id = NodeId::from(keypairs.remove(0).public());
    let mut nodes = build_nodes_from_keypairs(keypairs, 30000).await;
    let node_enrs: Vec<Enr<CombinedKey>> = nodes.iter().map(|n| n.local_enr()).collect();

    // link the nodes together
    for (node, previous_node_enr) in nodes.iter_mut().skip(1).zip(node_enrs.clone()) {
        let key: Key<NodeId> = node.local_enr().node_id().into();
        let distance = key.log2_distance(&previous_node_enr.node_id().into()).unwrap();
        println!("Distance of node relative to next: {distance}");
        node.add_enr(previous_node_enr).unwrap();
    }

    // start a query on the last node
    let found_nodes = nodes.last_mut().unwrap().find_node(target_node_id).await.unwrap();

    // build expectations
    let expected_node_ids: Vec<NodeId> =
        node_enrs.iter().map(|enr| enr.node_id()).take(total_nodes - 1).collect();

    // NOTE: The number of peers found is statistical, as we only ask
    // peers for specific buckets, there is a chance our node doesn't
    // exist if the first few buckets asked for.
    println!(
        "Query with found {} peers. Total peers were: {}",
        found_nodes.len(),
        expected_node_ids.len()
    );
    assert_eq!(found_nodes.len(), expected_node_ids.len());
}

/// Run a query where the target is one of the nodes. We expect to result to return the target.
#[tokio::test]
async fn test_findnode_query_with_target() {
    init();
    // build a collection of 8 nodes
    let total_nodes = 8;
    // Seed is chosen for a linear topology. Each node is connected to each other and in the top 3
    // buckets from each other and the target.
    let keypairs = generate_deterministic_keypair(total_nodes + 1, 5);
    let target_node_id = NodeId::from(keypairs[0].public());
    let mut nodes = build_nodes_from_keypairs(keypairs, 40150).await;
    let node_enrs: Vec<Enr<CombinedKey>> = nodes.iter().map(|n| n.local_enr()).collect();

    // link the nodes together
    for (node, previous_node_enr) in nodes.iter_mut().skip(1).zip(node_enrs.clone()) {
        let key: Key<NodeId> = node.local_enr().node_id().into();
        let distance = key.log2_distance(&previous_node_enr.node_id().into()).unwrap();
        println!(
            "Distance of node: {} relative to next node:{} is:{}",
            previous_node_enr.node_id(),
            node.local_enr().node_id(),
            distance
        );
        node.add_enr(previous_node_enr).unwrap();
    }

    // start a query on the last node
    let found_nodes = nodes.last_mut().unwrap().find_node(target_node_id).await.unwrap();

    println!("Query found {} peers. Total peers were: {}", found_nodes.len(), nodes.len() - 1);

    assert!(found_nodes.iter().any(|enr| enr.node_id() == target_node_id));
}

#[tokio::test]
async fn test_predicate_search() {
    init();
    let total_nodes = 10;
    // Seed is chosen such that all nodes are in the 256th bucket of bootstrap
    let seed = 1652;
    // Generate `num_nodes` + bootstrap_node and target_node keypairs from given seed
    let keypairs = generate_deterministic_keypair(total_nodes + 2, seed);
    let mut nodes = build_nodes_from_keypairs(keypairs, 1500).await;
    // Last node is bootstrap node in a star topology
    let bootstrap_node = nodes.remove(0);
    // target_node is not polled.
    let target_node = nodes.pop().unwrap();

    // Update `num_nodes` with the required attnet value
    let num_nodes = total_nodes / 2;

    let required_attnet_value = Bytes::copy_from_slice(&[1, 0, 0, 0]);
    let unwanted_attnet_value = Bytes::copy_from_slice(&[0, 0, 0, 0]);
    println!("Bootstrap node: {}", bootstrap_node.local_enr().node_id());
    println!("Target node: {}", target_node.local_enr().node_id());

    for (i, swarm) in nodes.iter_mut().enumerate() {
        let key: Key<NodeId> = swarm.local_enr().node_id().into();
        let distance = key.log2_distance(&bootstrap_node.local_enr().node_id().into()).unwrap();
        println!(
            "Distance of local node {} relative to node {}: {}",
            swarm.local_enr().node_id(),
            bootstrap_node.local_enr().node_id(),
            distance
        );
        swarm.add_enr(bootstrap_node.local_enr().clone()).unwrap();
        if i % 2 == 0 {
            update_enr(swarm, "attnets", &unwanted_attnet_value);
        } else {
            update_enr(swarm, "attnets", &required_attnet_value);
        }
        bootstrap_node.add_enr(swarm.local_enr().clone()).unwrap();
    }

    // Predicate function for filtering enrs
    let predicate = move |enr: &Enr<CombinedKey>| {
        if let Some(Ok(v)) = enr.get_decodable::<Bytes>("attnets") {
            v == required_attnet_value.to_vec().as_slice()
        } else {
            false
        }
    };
    nodes.push(bootstrap_node);

    // Start a find enr predicate query
    let target_random_node_id = target_node.local_enr().node_id();
    let found_nodes = nodes
        .first_mut()
        .unwrap()
        .find_node_predicate(target_random_node_id, Box::new(predicate), total_nodes)
        .await
        .unwrap();

    println!("Query found {} peers. Total peers were: {}", found_nodes.len(), total_nodes,);
    println!("Nodes expected to pass predicate search {num_nodes}");
    assert_eq!(found_nodes.len(), num_nodes);
}

// The kbuckets table can have maximum 10 nodes in the same /24 subnet across all buckets
#[tokio::test]
async fn test_table_limits() {
    // this seed generates 12 node id's that are distributed across buckets such that no more than
    // 2 exist in a single bucket.
    let mut keypairs = generate_deterministic_keypair(12, 9487);
    let ip: Ipv4Addr = "127.0.0.1".parse().unwrap();
    let enr_key: CombinedKey = keypairs.remove(0);
    let enr = Enr::builder().ip4(ip).udp4(9050).build(&enr_key).unwrap();
    let listen_config = ListenConfig::Ipv4 { ip: enr.ip4().unwrap(), port: enr.udp4().unwrap() };
    let config = ConfigBuilder::new(listen_config).ip_limit().build();

    // let socket_addr = enr.udp_socket().unwrap();
    let discv5: Discv5 = Discv5::new(enr, enr_key, config).unwrap();
    let table_limit: usize = 10;
    // Generate `table_limit + 2` nodes in the same subnet.
    let enrs: Vec<Enr<CombinedKey>> = (1..=table_limit + 1)
        .map(|i| {
            let ip: Ipv4Addr = Ipv4Addr::new(192, 168, 1, i as u8);
            let enr_key: CombinedKey = keypairs.remove(0);
            Enr::builder().ip4(ip).udp4(9050 + i as u16).build(&enr_key).unwrap()
        })
        .collect();
    for enr in enrs {
        let _ = discv5.add_enr(enr.clone()); // we expect some of these to fail the filter.
    }
    // Number of entries should be `table_limit`, i.e one node got restricted
    assert_eq!(discv5.kbuckets.read().iter_ref().count(), table_limit);
}

// Each bucket can have maximum 2 nodes in the same /24 subnet
#[tokio::test]
async fn test_bucket_limits() {
    let enr_key = CombinedKey::generate_secp256k1();
    let ip: Ipv4Addr = "127.0.0.1".parse().unwrap();
    let enr = Enr::builder().ip4(ip).udp4(9500).build(&enr_key).unwrap();
    let bucket_limit: usize = 2;
    // Generate `bucket_limit + 1` keypairs that go in `enr` node's 256th bucket.
    let keys = {
        let mut keys = Vec::new();
        for _ in 0..bucket_limit + 1 {
            loop {
                let key = CombinedKey::generate_secp256k1();
                let enr_new = Enr::empty(&key).unwrap();
                let node_key: Key<NodeId> = enr.node_id().into();
                let distance = node_key.log2_distance(&enr_new.node_id().into()).unwrap();
                if distance == 256 {
                    keys.push(key);
                    break;
                }
            }
        }
        keys
    };
    // Generate `bucket_limit + 1` nodes in the same subnet.
    let enrs: Vec<Enr<CombinedKey>> = (1..=bucket_limit + 1)
        .map(|i| {
            let kp = &keys[i - 1];
            let ip: Ipv4Addr = Ipv4Addr::new(192, 168, 1, i as u8);
            Enr::builder().ip4(ip).udp4(9500 + i as u16).build(kp).unwrap()
        })
        .collect();

    let listen_config = ListenConfig::Ipv4 { ip: enr.ip4().unwrap(), port: enr.udp4().unwrap() };
    let config = ConfigBuilder::new(listen_config).ip_limit().build();

    let discv5: Discv5 = Discv5::new(enr, enr_key, config).unwrap();
    for enr in enrs {
        let _ = discv5.add_enr(enr.clone()); // we expect some of these to fail based on the filter.
    }

    // Number of entries should be equal to `bucket_limit`.
    assert_eq!(discv5.kbuckets.read().iter_ref().count(), bucket_limit);
}
