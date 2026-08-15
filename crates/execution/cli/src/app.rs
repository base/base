#[cfg(feature = "edge-measurement")]
use std::time::Duration;
use std::{fmt, sync::Arc};

use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseExecutorProvider;
use base_node_core::BaseNode;
use eyre::{Result, eyre};
use reth_cli_commands::launcher::Launcher;
use reth_cli_runner::CliRunner;
#[cfg(feature = "edge-measurement")]
use reth_cli_runner::CliRunnerConfig;
use reth_node_core::args::{OtlpInitStatus, OtlpLogsStatus};
use reth_node_metrics::recorder::install_prometheus_recorder;
use reth_rpc_server_types::RpcModuleValidator;
use reth_tracing::{Layers, TracingGuards};
use tracing::{info, warn};

use crate::{Cli, Commands};
#[cfg(feature = "edge-measurement")]
macro_rules! edge_node_result_outer_arm {
    ($result:expr) => {{
        let result = $result;
        let recorder = {
            let mut slot = crate::mev_trader::EDGE_NODE_RESULT_SLOT_V1
                .get_or_init(|| {
                    std::sync::Mutex::new(
                        crate::mev_trader::EdgeNodeResultSlotV1::VacantBeforeAdmission,
                    )
                })
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let recorder = match &*slot {
                crate::mev_trader::EdgeNodeResultSlotV1::Installed(recorder) => {
                    Some(std::sync::Arc::clone(recorder))
                }
                crate::mev_trader::EdgeNodeResultSlotV1::VacantBeforeAdmission
                | crate::mev_trader::EdgeNodeResultSlotV1::Consumed => None,
            };
            *slot = crate::mev_trader::EdgeNodeResultSlotV1::Consumed;
            recorder
        };
        if result.is_err() {
            let oob_recorded = recorder.is_some_and(|recorder| {
                recorder.recorder.prepare_cutoff();
                recorder
                    .sink
                    .record("shutdown", "NodeCommandErrBeforeGracefulShutdown", None)
                    .is_ok()
            });
            tracing::error!(
                stage = "shutdown",
                reason = "NodeCommandErrBeforeGracefulShutdown",
                oob_recorded = oob_recorded,
                "node command failed before graceful shutdown"
            );
        }
        result
    }};
}

/// A wrapper around a parsed CLI that handles command execution.
#[derive(Debug)]
pub struct CliApp<Ext: clap::Args + fmt::Debug, Rpc: RpcModuleValidator> {
    cli: Cli<Ext, Rpc>,
    runner: Option<CliRunner>,
    layers: Option<Layers>,
    guard: Option<TracingGuards>,
}

impl<Ext, Rpc> CliApp<Ext, Rpc>
where
    Ext: clap::Args + fmt::Debug,
    Rpc: RpcModuleValidator,
{
    pub(crate) fn new(cli: Cli<Ext, Rpc>) -> Self {
        Self { cli, runner: None, layers: Some(Layers::new()), guard: None }
    }

    /// Creates the default CLI runner.
    ///
    /// Edge measurement builds allow 240 seconds for graceful shutdown.
    pub fn default_runner() -> Result<CliRunner> {
        let runner = CliRunner::try_default_runtime()?;
        #[cfg(feature = "edge-measurement")]
        let runner = runner.with_config(
            CliRunnerConfig::new().with_graceful_shutdown_timeout(Duration::from_secs(240)),
        );
        Ok(runner)
    }

    /// Sets the runner for the CLI commander.
    ///
    /// This replaces any existing runner with the provided one.
    pub fn set_runner(&mut self, runner: CliRunner) {
        self.runner = Some(runner);
    }

    /// Access to tracing layers.
    ///
    /// Returns a mutable reference to the tracing layers, or error
    /// if tracing initialized and layers have detached already.
    pub fn access_tracing_layers(&mut self) -> Result<&mut Layers> {
        self.layers.as_mut().ok_or_else(|| eyre!("Tracing already initialized"))
    }

    /// Execute the configured cli command.
    ///
    /// This accepts a closure that is used to launch the node via the
    /// [`NodeCommand`](reth_cli_commands::node::NodeCommand).
    pub fn run(
        mut self,
        launcher: impl Launcher<crate::chainspec::BaseChainSpecParser, Ext>,
    ) -> Result<()> {
        let runner = match self.runner.take() {
            Some(runner) => runner,
            None => Self::default_runner()?,
        };

        // add network name to logs dir
        // Add network name if available to the logs dir
        if let Some(chain_spec) = self.cli.command.chain_spec() {
            self.cli.logs.log_file_directory =
                self.cli.logs.log_file_directory.join(chain_spec.chain.to_string());
        }

        self.init_tracing(&runner)?;

        // Install the prometheus recorder to be sure to record all metrics
        install_prometheus_recorder();

        let components = |spec: Arc<BaseChainSpec>| {
            (
                BaseExecutorProvider::base(Arc::clone(&spec)),
                Arc::new(BaseBeaconConsensus::new(spec)),
            )
        };

        match self.cli.command {
            Commands::Node(command) => {
                // Validate RPC modules using the configured validator
                if let Some(http_api) = &command.rpc.http_api {
                    Rpc::validate_selection(http_api, "http.api").map_err(|e| eyre!("{e}"))?;
                }
                if let Some(ws_api) = &command.rpc.ws_api {
                    Rpc::validate_selection(ws_api, "ws.api").map_err(|e| eyre!("{e}"))?;
                }

                let result = runner.run_command_until_exit(|ctx| command.execute(ctx, launcher));
                #[cfg(feature = "edge-measurement")]
                let result = edge_node_result_outer_arm!(result);
                result
            }
            Commands::Init(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Commands::InitState(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Commands::DumpGenesis(command) => runner.run_blocking_until_ctrl_c(command.execute()),
            Commands::Db(command) => {
                runner.run_blocking_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            Commands::Stage(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode, _>(ctx, components))
            }
            Commands::P2P(command) => runner.run_until_ctrl_c(command.execute::<BaseNode>()),
            Commands::Config(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::Prune(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            #[cfg(feature = "dev")]
            Commands::TestVectors(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::ReExecute(command) => {
                let runtime = runner.runtime();
                runner.run_until_ctrl_c(command.execute::<BaseNode>(components, runtime))
            }
            Commands::BaseProofs(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Commands::SnapshotManifest(command) => {
                command.execute()?;
                Ok(())
            }
            Commands::Download(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>())
            }
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, this function stores guard to the struct.
    /// For gRPC OTLP, it requires tokio runtime context.
    pub fn init_tracing(&mut self, runner: &CliRunner) -> Result<()> {
        if self.guard.is_none() {
            let mut layers = self.layers.take().unwrap_or_default();

            let otlp_status = runner.block_on(self.cli.traces.init_otlp_tracing(&mut layers))?;
            let otlp_logs_status = runner.block_on(self.cli.traces.init_otlp_logs(&mut layers))?;

            let enable_reload = self.cli.command.debug_namespace_enabled();
            self.guard = Some(self.cli.logs.init_tracing_with_layers(layers, enable_reload)?);
            info!(target: "reth::cli", log_dir = %self.cli.logs.log_file_directory, "Initialized tracing");

            match otlp_status {
                OtlpInitStatus::Started(endpoint) => {
                    info!(target: "reth::cli", protocol = ?self.cli.traces.protocol, endpoint = %endpoint, "Started OTLP tracing export");
                }
                OtlpInitStatus::NoFeature => {
                    warn!(target: "reth::cli", "Provided OTLP tracing arguments do not have effect, compile with the `otlp` feature")
                }
                OtlpInitStatus::Disabled => {}
            }

            match otlp_logs_status {
                OtlpLogsStatus::Started(endpoint) => {
                    info!(target: "reth::cli", protocol = ?self.cli.traces.protocol, endpoint = %endpoint, "Started OTLP logs export");
                }
                OtlpLogsStatus::NoFeature => {
                    warn!(target: "reth::cli", "Provided OTLP logs arguments do not have effect, compile with the `otlp-logs` feature")
                }
                OtlpLogsStatus::Disabled => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "edge-measurement")]
    use std::{
        collections::BTreeMap,
        fmt, fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(feature = "edge-measurement")]
    use base_flashblocks::{
        EdgeMeasurementAdmissionStateV1, EdgeMeasurementAuthorityAuditSnapshotV1,
        EdgeMeasurementRecorderV1,
    };
    #[cfg(feature = "edge-measurement")]
    use tracing::{
        Event, Id, Level, Metadata, Subscriber,
        field::{Field, Visit},
        span::{Attributes, Record},
    };

    #[cfg(feature = "edge-measurement")]
    use crate::mev_trader::{
        EdgeOobFailureSinkAuditSnapshotV1, EdgeOobFailureSinkV1, edge_node_result_test_consumed_v1,
        edge_node_result_test_install_v1, edge_node_result_test_lock_v1,
        edge_node_result_test_oob_sink_v1, edge_node_result_test_poison_v1,
        edge_node_result_test_reset_v1,
    };

    #[cfg(feature = "edge-measurement")]
    #[derive(Default)]
    struct NodeResultEventCapture {
        events: Mutex<Vec<(Level, BTreeMap<String, String>)>>,
    }

    #[cfg(feature = "edge-measurement")]
    impl NodeResultEventCapture {
        fn snapshot(&self) -> Vec<(Level, BTreeMap<String, String>)> {
            self.events.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
        }
    }

    #[cfg(feature = "edge-measurement")]
    struct NodeResultEventSubscriber {
        capture: Arc<NodeResultEventCapture>,
    }

    #[cfg(feature = "edge-measurement")]
    impl Subscriber for NodeResultEventSubscriber {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut fields = BTreeMap::new();
            event.record(&mut NodeResultEventVisitor { fields: &mut fields });
            self.capture
                .events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((*event.metadata().level(), fields));
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    #[cfg(feature = "edge-measurement")]
    struct NodeResultEventVisitor<'a> {
        fields: &'a mut BTreeMap<String, String>,
    }

    #[cfg(feature = "edge-measurement")]
    impl Visit for NodeResultEventVisitor<'_> {
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.fields.insert(field.name().to_owned(), value.to_string());
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.fields.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    #[cfg(feature = "edge-measurement")]
    fn node_result_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "edge-app-result-{label}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock").as_nanos()
        ));
        fs::create_dir(&root).expect("node-result root");
        root
    }

    #[cfg(feature = "edge-measurement")]
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecorderAuthoritySnapshot {
        authority: EdgeMeasurementAuthorityAuditSnapshotV1,
        canonical_files: BTreeMap<String, Vec<u8>>,
        oob: EdgeOobFailureSinkAuditSnapshotV1,
    }

    #[cfg(feature = "edge-measurement")]
    fn recorder_authority_snapshot(
        root: &Path,
        recorder: &EdgeMeasurementRecorderV1,
        oob_sink: &EdgeOobFailureSinkV1,
    ) -> RecorderAuthoritySnapshot {
        let mut canonical_files = BTreeMap::new();
        for entry in fs::read_dir(root).expect("authority directory") {
            let entry = entry.expect("authority directory entry");
            assert!(entry.file_type().expect("authority entry type").is_file());
            let name = entry.file_name().into_string().expect("ASCII authority filename");
            if name != "edge-writer-failures-v1.ndjson" {
                let bytes = fs::read(entry.path()).expect("authority file bytes");
                assert!(canonical_files.insert(name, bytes).is_none());
            }
        }
        RecorderAuthoritySnapshot {
            authority: recorder.authority_audit_snapshot(),
            canonical_files,
            oob: oob_sink.audit_snapshot().expect("OOB authority audit"),
        }
    }

    #[cfg(feature = "edge-measurement")]
    fn assert_pre_terminal_err_delta(
        before: &RecorderAuthoritySnapshot,
        after: &RecorderAuthoritySnapshot,
    ) {
        assert_eq!(before.authority.admission_state(), EdgeMeasurementAdmissionStateV1::Open);
        assert_eq!(after.authority.admission_state(), EdgeMeasurementAdmissionStateV1::Cutoff);
        assert!(
            after.authority.is_exact_initial_cutoff_delta_from(&before.authority),
            "complete non-fence recorder and registry authority must remain byte-exact"
        );
        assert_eq!(before.authority.cutoff_record(), None);
        assert_eq!(after.authority.cutoff_record(), None);
        assert!(before.authority.registry().admission_open());
        assert!(!before.authority.registry().measurement_closed());
        assert!(!after.authority.registry().admission_open());
        assert!(after.authority.registry().measurement_closed());
        assert_eq!(
            after.authority.registry().revision(),
            before
                .authority
                .registry()
                .revision()
                .checked_add(1)
                .expect("registry cutoff revision")
        );
        assert_eq!(after.canonical_files, before.canonical_files);

        assert_eq!(after.oob.writer_instance_id, before.oob.writer_instance_id);
        assert_eq!(after.oob.producer_epoch, before.oob.producer_epoch);
        assert_eq!(before.oob.next_ordinal, 0);
        assert_eq!(after.oob.next_ordinal, 1);
        assert!(before.oob.file_bytes.is_empty());
        assert_eq!(before.oob.file_metadata.len, 0);
        assert_eq!(usize::try_from(after.oob.file_metadata.len), Ok(after.oob.file_bytes.len()));
        assert_eq!(after.oob.file_metadata.mode, before.oob.file_metadata.mode);
        assert_eq!(after.oob.file_metadata.uid, before.oob.file_metadata.uid);
        assert_eq!(after.oob.file_metadata.gid, before.oob.file_metadata.gid);
        assert_eq!(after.oob.file_metadata.device, before.oob.file_metadata.device);
        assert_eq!(after.oob.file_metadata.inode, before.oob.file_metadata.inode);
        assert_eq!(after.oob.file_metadata.hard_links, before.oob.file_metadata.hard_links);
        assert!(
            (
                after.oob.file_metadata.modified_seconds,
                after.oob.file_metadata.modified_nanoseconds,
            ) >= (
                before.oob.file_metadata.modified_seconds,
                before.oob.file_metadata.modified_nanoseconds,
            )
        );
        assert!(
            (after.oob.file_metadata.changed_seconds, after.oob.file_metadata.changed_nanoseconds,)
                >= (
                    before.oob.file_metadata.changed_seconds,
                    before.oob.file_metadata.changed_nanoseconds,
                )
        );

        let lines = after
            .oob
            .file_bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert_eq!(after.oob.file_bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        let line: serde_json::Value =
            serde_json::from_slice(lines[0]).expect("canonical Err OOB line");
        assert_eq!(line.as_object().expect("OOB object").len(), 9);
        assert_eq!(line["stage"], "shutdown");
        assert_eq!(line["reason"], "NodeCommandErrBeforeGracefulShutdown");
        assert_eq!(line["ordinal"], "0");
        assert_eq!(line["producerEpoch"], "2");
        assert_eq!(line["writerInstanceId"], after.oob.writer_instance_id);
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn node_command_execute_outer_result_arm_source_seal() {
        const APP_SOURCE: &str = include_str!("app.rs");
        const NODE_ARM_START: &str = concat!("            Commands::", "Node(command) => {\n");
        const NEXT_ARM_START: &str = concat!("            Commands::", "Init(command) => {\n");
        const OUTER_RESULT_SEQUENCE: &str = concat!(
            "                let result = runner.run_command_until_exit(|ctx| command.",
            "execute(ctx, launcher));\n",
            "                #[cfg(feature = \"edge-measurement\")]\n",
            "                let result = edge_node_result_outer_",
            "arm!(result);\n",
            "                result\n",
        );
        const OUTER_RESULT_MACRO_CALL: &str = concat!("edge_node_result_outer_", "arm!(");

        assert_eq!(APP_SOURCE.matches(NODE_ARM_START).count(), 1);
        assert_eq!(APP_SOURCE.matches(NEXT_ARM_START).count(), 1);
        assert_eq!(APP_SOURCE.matches(OUTER_RESULT_SEQUENCE).count(), 1);

        let (_, after_node_arm_start) =
            APP_SOURCE.split_once(NODE_ARM_START).expect("real Node command arm");
        let (node_arm, _) = after_node_arm_start
            .split_once(NEXT_ARM_START)
            .expect("Node command arm must precede Init command arm");
        assert_eq!(node_arm.matches(OUTER_RESULT_SEQUENCE).count(), 1);
        assert_eq!(node_arm.matches(OUTER_RESULT_MACRO_CALL).count(), 1);
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn app_outer_result_arm_uses_real_static_once_and_preserves_identity() {
        let _serial = edge_node_result_test_lock_v1();
        assert_eq!(EdgeMeasurementAuthorityAuditSnapshotV1::RECORDER_STATE_FIELD_COUNT, 49);
        assert_eq!(EdgeMeasurementAuthorityAuditSnapshotV1::REGISTRY_FIELD_COUNT, 32);
        assert_eq!(EdgeOobFailureSinkAuditSnapshotV1::COVERED_FIELD_COUNT, 5);
        assert_eq!(EdgeOobFailureSinkAuditSnapshotV1::FILE_METADATA_FIELD_COUNT, 11);

        edge_node_result_test_reset_v1();
        let ok_root = node_result_root("ok");
        let ok_recorder = edge_node_result_test_install_v1(&ok_root, 1);
        let ok_oob_sink = edge_node_result_test_oob_sink_v1();
        let ok_before = recorder_authority_snapshot(&ok_root, &ok_recorder, &ok_oob_sink);
        let ok_marker = Box::new(17_u64);
        let ok_address = (&*ok_marker) as *const u64;
        let ok_capture = Arc::new(NodeResultEventCapture::default());
        let ok: Result<Box<u64>, &'static str> = tracing::subscriber::with_default(
            NodeResultEventSubscriber { capture: Arc::clone(&ok_capture) },
            || edge_node_result_outer_arm!(Ok(ok_marker)),
        );
        assert_eq!((&**ok.as_ref().expect("same Ok")) as *const u64, ok_address);
        assert!(ok_capture.snapshot().is_empty());
        assert!(edge_node_result_test_consumed_v1());
        let ok_after = recorder_authority_snapshot(&ok_root, &ok_recorder, &ok_oob_sink);
        assert_eq!(ok_after, ok_before);
        edge_node_result_test_reset_v1();
        fs::remove_dir_all(ok_root).expect("Ok root cleanup");

        let error_root = node_result_root("error");
        let error_recorder = edge_node_result_test_install_v1(&error_root, 2);
        let error_oob_sink = edge_node_result_test_oob_sink_v1();
        let error_before =
            recorder_authority_snapshot(&error_root, &error_recorder, &error_oob_sink);
        let error = String::from("original error");
        let error_identity = (error.as_ptr(), error.len(), error.capacity());
        let error_capture = Arc::new(NodeResultEventCapture::default());
        let result: Result<(), String> = tracing::subscriber::with_default(
            NodeResultEventSubscriber { capture: Arc::clone(&error_capture) },
            || edge_node_result_outer_arm!(Err(error)),
        );
        let returned_error = result.as_ref().expect_err("same Err");
        assert_eq!(
            (returned_error.as_ptr(), returned_error.len(), returned_error.capacity()),
            error_identity
        );
        assert_eq!(returned_error, "original error");
        let events = error_capture.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].1,
            BTreeMap::from([
                ("message".to_owned(), "node command failed before graceful shutdown".to_owned(),),
                ("oob_recorded".to_owned(), "true".to_owned()),
                ("reason".to_owned(), "NodeCommandErrBeforeGracefulShutdown".to_owned()),
                ("stage".to_owned(), "shutdown".to_owned()),
            ])
        );
        assert_eq!(events[0].0, Level::ERROR);
        assert!(edge_node_result_test_consumed_v1());
        let error_after =
            recorder_authority_snapshot(&error_root, &error_recorder, &error_oob_sink);
        assert_pre_terminal_err_delta(&error_before, &error_after);
        edge_node_result_test_reset_v1();
        fs::remove_dir_all(error_root).expect("Err root cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn real_static_poison_recovers_through_production_app_arm() {
        let _serial = edge_node_result_test_lock_v1();
        edge_node_result_test_poison_v1();
        let root = node_result_root("poison");
        let recorder = edge_node_result_test_install_v1(&root, 3);
        let counters_before = recorder.source_final_counters();
        let error = Box::new(29_u64);
        let error_address = (&*error) as *const u64;
        let result: Result<(), Box<u64>> = edge_node_result_outer_arm!(Err(error));
        assert_eq!((&**result.as_ref().expect_err("same Err")) as *const u64, error_address);
        assert!(edge_node_result_test_consumed_v1());
        assert!(recorder.cutoff_latched());
        assert_eq!(recorder.source_final_counters(), counters_before);
        let oob =
            fs::read(root.join("edge-writer-failures-v1.ndjson")).expect("poison-recovery OOB");
        assert_eq!(oob.iter().filter(|byte| **byte == b'\n').count(), 1);
        edge_node_result_test_reset_v1();
        fs::remove_dir_all(root).expect("poison root cleanup");
    }
}
