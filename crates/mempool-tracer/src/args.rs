// #[derive(Clone, Debug, PartialEq, Eq)]
// pub enum TracingType {
//     LOGGING,
//     DATABASE
// }

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct MempoolTracerArgs {
    #[arg(long = "rollup.mempooltracer.enabled", value_name = "ROLLUP_MEMPOOL_TRACER_ENABLED")]
    pub mempool_tracer_enabled: bool,

    // #[arg(long = "rollup.mempooltracer.type", value_name = "ROLLUP_MEMPOOL_TRACER_TYPE")]
    // pub destination: TracingType,

}