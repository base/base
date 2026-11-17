//! Answers `systemConfig` requests: L1 `ConfigUpdate` logs applied to a system config the way
//! derivation applies them.

use std::{collections::BTreeMap, slice};

use base_common_chains::BaseFeature;
use base_common_genesis::SystemConfig;
use base_diff_fuzz::{
    ConfigLogOutcome, ConfigLogResult, Request, Response, SystemConfigOutput, SystemConfigValues,
};
use eyre::{bail, ensure, eyre};

use crate::DiffAdapter;

impl DiffAdapter {
    /// Applies the request's receipts with [`SystemConfig::update_with_receipts`], the function
    /// derivation calls on the receipts of each new L1 origin, with Ecotone decided by the L1
    /// timestamp as derivation decides it.
    ///
    /// Each log is applied on its own, in a receipt with its receipt's status, to learn its
    /// outcome; the receipts are then applied all at once, and both results must agree.
    pub fn system_config(&self, request: &Request) -> eyre::Result<Response> {
        let input = request
            .system_config
            .as_ref()
            .ok_or_else(|| eyre!("systemConfig request has no systemConfig"))?;
        for (index, receipt) in input.receipts.iter().enumerate() {
            ensure!(receipt.status <= 1, "receipts[{index}] has status {}", receipt.status);
        }
        let address = self.rollup_config.l1_system_config_address;
        let ecotone = self
            .rollup_config
            .is_feature_active_cascading(BaseFeature::BlobAwareL1Fees, input.l1_timestamp);
        let receipts: Vec<alloy_consensus::Receipt> =
            input.receipts.iter().map(Into::into).collect();

        let mut config = SystemConfig::from(&input.config);
        let mut logs = Vec::new();
        for receipt in &receipts {
            for log in &receipt.logs {
                let single = alloy_consensus::Receipt {
                    status: receipt.status,
                    cumulative_gas_used: 0,
                    logs: vec![log.clone()],
                };
                let (applied, errors) =
                    config.update_with_receipts(slice::from_ref(&single), address, ecotone);
                logs.push(match (applied.as_slice(), errors.as_slice()) {
                    ([kind], []) => ConfigLogResult {
                        outcome: ConfigLogOutcome::Applied,
                        kind: Some(*kind as u64),
                        error: None,
                    },
                    ([], [error]) => ConfigLogResult {
                        outcome: ConfigLogOutcome::Rejected,
                        kind: None,
                        error: Some(error.to_string()),
                    },
                    ([], []) => ConfigLogResult {
                        outcome: ConfigLogOutcome::Ignored,
                        kind: None,
                        error: None,
                    },
                    _ => bail!("one log gave updates {applied:?} and errors {errors:?}"),
                });
            }
        }
        let mut whole = SystemConfig::from(&input.config);
        whole.update_with_receipts(&receipts, address, ecotone);
        ensure!(
            whole == config,
            "applying logs one at a time gave {config:?}, all at once {whole:?}"
        );

        Ok(Response {
            id: request.id.clone(),
            ok: true,
            error: None,
            valid: true,
            invalid_reason: None,
            rejected: Vec::new(),
            header: None,
            computed_base_fee_per_gas: None,
            receipts: Vec::new(),
            post: BTreeMap::new(),
            coverage: None,
            system_config: Some(SystemConfigOutput {
                config: Some(SystemConfigValues::from(&config)),
                logs,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_diff_fuzz::{
        ConfigLogOutcome, ConfigLogs, ConfigUpdate, L1Receipt, Mode, Request, SystemConfigInput,
        SystemConfigValues,
    };

    use crate::DiffAdapter;

    #[test]
    fn reports_each_log_and_skips_malformed_ones() {
        let valid = ConfigUpdate::GasLimit(60_000_000).log();
        let mut bad_pointer = ConfigUpdate::MinBaseFee(7).log();
        let mut data = bad_pointer.data.to_vec();
        data[31] = 64;
        bad_pointer.data = data.into();
        let mut elsewhere = ConfigUpdate::MinBaseFee(9).log();
        elsewhere.address = Address::repeat_byte(1);
        let request = Request {
            id: "sysconfig".to_owned(),
            chain: "base-mainnet".to_owned(),
            mode: Mode::SystemConfig,
            ancestors: Vec::new(),
            header: Default::default(),
            transactions: Vec::new(),
            witness: Default::default(),
            trace: None,
            system_config: Some(SystemConfigInput {
                l1_timestamp: 1_800_000_000,
                forks: ConfigLogs::forks(1_800_000_000),
                config: SystemConfigValues { gas_limit: 1, min_base_fee: 3, ..Default::default() },
                receipts: vec![
                    L1Receipt { status: 1, logs: vec![valid, bad_pointer, elsewhere] },
                    L1Receipt { status: 0, logs: vec![ConfigUpdate::MinBaseFee(11).log()] },
                ],
            }),
        };
        let response = DiffAdapter::base_mainnet().execute(&request).unwrap();
        let output = response.system_config.unwrap();
        let outcomes: Vec<_> = output.logs.iter().map(|log| (log.outcome, log.kind)).collect();
        assert_eq!(
            outcomes,
            [
                (ConfigLogOutcome::Applied, Some(2)),
                (ConfigLogOutcome::Rejected, None),
                (ConfigLogOutcome::Ignored, None),
                (ConfigLogOutcome::Ignored, None),
            ]
        );
        let config = output.config.unwrap();
        assert_eq!((config.gas_limit, config.min_base_fee), (60_000_000, 3));
    }
}
