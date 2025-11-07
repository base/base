use alloy_sol_macro::sol;

sol! {
    type GameType is uint32;
    type Claim is bytes32;
    type Timestamp is uint64;
    type Hash is bytes32;

    #[sol(rpc)]
    #[derive(Debug)]
    contract DisputeGameFactory {
        /// @notice Emitted when a new dispute game is created.
        event DisputeGameCreated(address indexed disputeProxy, GameType indexed gameType, Claim indexed rootClaim);

        /// @notice `gameImpls` is a mapping that maps `GameType`s to their respective
        ///         `IDisputeGame` implementations.
        mapping(GameType => IDisputeGame) public gameImpls;

        /// @notice Returns the required bonds for initializing a dispute game of the given type.
        mapping(GameType => uint256) public initBonds;

        /// @notice The total number of dispute games created by this factory.
        function gameCount() external view returns (uint256 gameCount_);

        /// @notice `gameAtIndex` returns the dispute game contract address and its creation timestamp
        ///         at the given index. Each created dispute game increments the underlying index.
        function gameAtIndex(uint256 _index) external view returns (GameType gameType, Timestamp timestamp, IDisputeGame proxy);

        /// @notice Returns the dispute game metadata for a given UUID.
        function games(GameType gameType, Claim rootClaim, bytes extraData) external view returns (IDisputeGame proxy, Timestamp timestamp);

        /// @notice Returns the UUID for a given dispute game configuration.
        function getGameUUID(GameType gameType, Claim rootClaim, bytes extraData) external pure returns (Hash uuid);

        /// @notice Creates a new DisputeGame proxy contract.
        function create(GameType gameType, Claim rootClaim, bytes extraData) external;
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IDisputeGame {
        function status() external view returns (GameStatus status_);
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IFaultDisputeGame {
        function l2BlockNumber() external view returns (uint256 l2BlockNumber_);
    }

    #[sol(rpc)]
    contract OPSuccinctFaultDisputeGame {
        /// @notice Getter for the game type.
        function gameType() public pure returns (GameType gameType_);

        /// @notice Getter for the creator of the dispute game.
        function gameCreator() public pure returns (address creator_);

        /// @notice The L2 block number for which this game is proposing an output root.
        function l2BlockNumber() public pure returns (uint256 l2BlockNumber_);

        /// @notice Only the starting block number of the game.
        function startingBlockNumber() external view returns (uint256 startingBlockNumber_);

        /// @notice Getter for the root claim.
        function rootClaim() public pure returns (Claim rootClaim_);

        /// @notice Getter for the parent hash of the L1 block when the dispute game was created.
        function l1Head() public pure returns (Hash l1Head_);

        /// @notice Getter for the status of the game.
        function status() public view returns (GameStatus status_);

        /// @notice Getter for the claim data.
        function claimData() public view returns (ClaimData memory claimData_);

        /// @notice Getter for the was respected game type when created.
        function wasRespectedGameTypeWhenCreated() external view returns (bool wasRespectedGameTypeWhenCreated_);

        /// @notice Challenges the game.
        function challenge() external payable returns (ProposalStatus);
        /// @notice Proves the game.
        function prove(bytes calldata proofBytes) external returns (ProposalStatus);

        /// @notice Resolves the game after the clock expires.
        ///         `DEFENDER_WINS` when no one has challenged the proposer's claim and `MAX_CHALLENGE_DURATION` has passed
        ///         or there is a challenge but the prover has provided a valid proof within the `MAX_PROVE_DURATION`.
        ///         `CHALLENGER_WINS` when the proposer's claim has been challenged, but the proposer has not proven
        ///         its claim within the `MAX_PROVE_DURATION`.
        function resolve() external returns (GameStatus status_);

        /// @notice Determines if the game is finished.
        function gameOver() external view returns (bool gameOver_);

        /// @notice Returns the max challenge duration.
        function maxChallengeDuration() external view returns (uint256 maxChallengeDuration_);

        /// @notice Returns the anchor state registry contract.
        function anchorStateRegistry() external view returns (IAnchorStateRegistry registry_);

        /// @notice Returns the challenger bond amount.
        function challengerBond() external view returns (uint256 challengerBond_);

        /// @notice Claim the credit belonging to the recipient address.
        function claimCredit(address _recipient) external;

        /// @notice Returns the credit balance of a given recipient.
        function credit(address _recipient) external view returns (uint256 credit_);
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IAnchorStateRegistry {
        function anchorGame() external view returns (IDisputeGame anchorGame_);
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    contract AnchorStateRegistry {
        /// @notice Returns the current anchor root.
        function getAnchorRoot() public view returns (Hash, uint256);

        /// @notice Returns whether a game is finalized.
        function isGameFinalized(IDisputeGame _game) public view returns (bool);

        /// @notice Returns the current anchor game reference.
        function anchorGame() public view returns (IDisputeGame anchorGame_);
    }

    #[derive(Debug, PartialEq)]
    /// @notice The current status of the dispute game.
    enum GameStatus {
        // The game is currently in progress, and has not been resolved.
        IN_PROGRESS,
        // The game has concluded, and the `rootClaim` was challenged successfully.
        CHALLENGER_WINS,
        // The game has concluded, and the `rootClaim` could not be contested.
        DEFENDER_WINS
    }

    #[derive(Debug, PartialEq)]
    enum ProposalStatus {
        // The initial state of a new proposal.
        Unchallenged,
        // A proposal that has been challenged but not yet proven.
        Challenged,
        // An unchallenged proposal that has been proven valid with a verified proof.
        UnchallengedAndValidProofProvided,
        // A challenged proposal that has been proven valid with a verified proof.
        ChallengedAndValidProofProvided,
        // The final state after resolution, either GameStatus.CHALLENGER_WINS or GameStatus.DEFENDER_WINS.
        Resolved
    }

    #[derive(Debug)]
    /// @notice The `ClaimData` struct represents the data associated with a Claim.
    struct ClaimData {
        uint32 parentIndex;
        address counteredBy;
        address prover;
        Claim claim;
        ProposalStatus status;
        Timestamp deadline;
    }

    /// @notice The `L2Output` struct represents the L2 output.
    struct L2Output {
        uint64 zero;
        bytes32 l2_state_root;
        bytes32 l2_storage_hash;
        bytes32 l2_claim_hash;
    }
}
