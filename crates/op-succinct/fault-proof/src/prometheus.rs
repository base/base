use op_succinct_host_utils::metrics::MetricsGauge;
use strum::EnumMessage;
use strum_macros::{Display, EnumIter};

// Define an enum for all fault proof proposer metrics gauges.
#[derive(Debug, Clone, Copy, Display, EnumIter, EnumMessage)]
pub enum ProposerGauge {
    // Proposer metrics
    #[strum(
        serialize = "op_succinct_fp_finalized_l2_block_number",
        message = "Finalized L2 block number"
    )]
    FinalizedL2BlockNumber,
    #[strum(
        serialize = "op_succinct_fp_latest_game_l2_block_number",
        message = "Latest game L2 block number"
    )]
    LatestGameL2BlockNumber,
    #[strum(
        serialize = "op_succinct_fp_anchor_game_l2_block_number",
        message = "Anchor game L2 block number"
    )]
    AnchorGameL2BlockNumber,
    #[strum(
        serialize = "op_succinct_fp_games_created",
        message = "Total number of games created by the proposer"
    )]
    GamesCreated,
    #[strum(
        serialize = "op_succinct_fp_games_resolved",
        message = "Total number of games resolved by the proposer"
    )]
    GamesResolved,
    #[strum(
        serialize = "op_succinct_fp_games_bonds_claimed",
        message = "Total number of games that bonds were claimed by the proposer"
    )]
    GamesBondsClaimed,
    // Error metrics
    #[strum(
        serialize = "op_succinct_fp_errors",
        message = "Total number of errors encountered by the proposer"
    )]
    Errors,
}

impl MetricsGauge for ProposerGauge {}

// Define an enum for all fault proof challenger metrics gauges.
#[derive(Debug, Clone, Copy, Display, EnumIter, EnumMessage)]
pub enum ChallengerGauge {
    // Challenger metrics
    #[strum(
        serialize = "op_succinct_fp_challenger_games_challenged",
        message = "Total number of games challenged by the challenger"
    )]
    GamesChallenged,
    #[strum(
        serialize = "op_succinct_fp_challenger_games_resolved",
        message = "Total number of games resolved by the challenger"
    )]
    GamesResolved,
    #[strum(
        serialize = "op_succinct_fp_challenger_games_bonds_claimed",
        message = "Total number of games that bonds were claimed by the challenger"
    )]
    GamesBondsClaimed,
    // Error metrics
    #[strum(
        serialize = "op_succinct_fp_challenger_errors",
        message = "Total number of errors encountered by the challenger"
    )]
    Errors,
}

impl MetricsGauge for ChallengerGauge {}
