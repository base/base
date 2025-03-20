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
        serialize = "op_succinct_fp_game_creation_error",
        message = "Total number of game creation errors encountered by the proposer"
    )]
    GameCreationError,
    #[strum(
        serialize = "op_succinct_fp_game_defense_error",
        message = "Total number of game defense errors encountered by the proposer"
    )]
    GameDefenseError,
    #[strum(
        serialize = "op_succinct_fp_game_resolution_error",
        message = "Total number of game resolution errors encountered by the proposer"
    )]
    GameResolutionError,
    #[strum(
        serialize = "op_succinct_fp_bond_claiming_error",
        message = "Total number of bond claiming errors encountered by the proposer"
    )]
    BondClaimingError,
    #[strum(
        serialize = "op_succinct_fp_metrics_error",
        message = "Total number of metrics errors encountered by the proposer"
    )]
    MetricsError,
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
        serialize = "op_succinct_fp_challenger_game_challenging_error",
        message = "Total number of game challenging errors encountered by the challenger"
    )]
    GameChallengingError,
    #[strum(
        serialize = "op_succinct_fp_challenger_game_resolution_error",
        message = "Total number of game resolution errors encountered by the challenger"
    )]
    GameResolutionError,
    #[strum(
        serialize = "op_succinct_fp_challenger_bond_claiming_error",
        message = "Total number of bond claiming errors encountered by the challenger"
    )]
    BondClaimingError,
}

impl MetricsGauge for ChallengerGauge {}
