use std::net::Ipv4Addr;

use enr::Enr;

// Ensures the mock data is not used in the production environment.
// See unit test `test_secp256k1_sign_ecdsa_with_mock_noncedata` for details.
#[test]
fn test_secp256k1_sign_ecdsa_with_noncedata() {
    let not_expected_enr_base64 = "enr:-IS4QLJYdRwxdy-AbzWC6wL9ooB6O6uvCvJsJ36rbJztiAs1JzPY0__YkgFzZwNUuNhm1BDN6c4-UVRCJP9bXNCmoDYBgmlkgnY0gmlwhH8AAAGJc2VjcDI1NmsxoQPKY0yuDUmstAHYpMa2_oxVtw0RW_QAdpzBQA8yWM0xOIN1ZHCCdl8";

    let key_data =
        hex::decode("b71c71a67e1177ad4e901695e1b4b9ee17ae16c6668d313eac2f96dbcda3f291").unwrap();
    let ip = Ipv4Addr::new(127, 0, 0, 1);
    let udp = 30303;

    let key = secp256k1::SecretKey::from_slice(&key_data).unwrap();
    let enr = Enr::builder().ip4(ip).udp4(udp).build(&key).unwrap();
    let enr_base64 = enr.to_base64();
    assert_ne!(enr_base64, not_expected_enr_base64);

    let enr = enr_base64.parse::<Enr<secp256k1::SecretKey>>().unwrap();
    assert!(enr.verify());
}
