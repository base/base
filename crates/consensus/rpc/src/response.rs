//! Response to safe head request

#[cfg(test)]
mod tests {
    use base_consensus_safedb::SafeHeadResponse;

    // <https://github.com/alloy-rs/op-alloy/issues/155>
    #[test]
    fn test_safe_head_response() {
        let s = r#"{"l1Block":{"hash":"0x7de331305c2bb3e5642a2adcb9c003cc67cefc7b05a3da5a6a4b12cf3af15407","number":6834391},"safeHead":{"hash":"0xa5e5ec1ade7d6fef209f73861bf0080950cde74c4b0c07823983eb5225e282a8","number":18266679}}"#;
        let _response: SafeHeadResponse = serde_json::from_str(s).unwrap();
    }
}
