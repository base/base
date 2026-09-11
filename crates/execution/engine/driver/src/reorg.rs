//! Stream wrapper that simulates reorgs.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    task::{Context, Poll, ready},
};

use alloy_primitives::Bytes;
use base_common_chain_config::ChainSpecProvider;
use base_common_types_chain::{
    BlockBodyExt as _, BlockHeader, SealedBlock, SignedTransaction, Transaction,
};
use base_common_types_payload::{BaseBuiltPayload, ExecutionCommand, ForkchoiceState};
use base_execution_evm_blocks::{BaseEvmConfig, BlockBuilderOutcome};
use base_execution_evm_runtime::{BlockExecutionError, BlockValidationError, State};
use base_execution_payload::BaseEngineValidator;
use base_execution_state_types::{BlockReader, ProviderError, StateProviderFactory};
use futures::{Stream, StreamExt, stream::FuturesUnordered};
use tokio::sync::oneshot;
use tracing::{debug, error, trace};

#[derive(Debug)]
enum EngineReorgState {
    Forward,
    Reorg { queue: VecDeque<ExecutionCommand> },
}

/// Completion of an injected authoritative append.
pub type EngineReorgResponse = Result<
    Result<
        base_common_types_payload::HeadUpdateOutcome,
        base_common_types_payload::AppendPayloadError,
    >,
    oneshot::error::RecvError,
>;

type ReorgResponseFut = Pin<Box<dyn Future<Output = EngineReorgResponse> + Send + Sync>>;

/// Engine API stream wrapper that simulates reorgs with specified frequency.
#[derive(Debug)]
#[pin_project::pin_project]
pub struct EngineReorg<S, Provider> {
    /// Underlying stream
    #[pin]
    stream: S,
    /// Database provider.
    provider: Provider,
    /// Evm configuration.
    evm_config: BaseEvmConfig,
    /// Payload validator.
    payload_validator: BaseEngineValidator,
    /// The frequency of reorgs.
    frequency: usize,
    /// The depth of reorgs.
    depth: usize,
    /// The number of forwarded forkchoice states.
    /// This is reset after a reorg.
    forkchoice_states_forwarded: usize,
    /// Current state of the stream.
    state: EngineReorgState,
    /// Last forkchoice state.
    last_forkchoice_state: Option<ForkchoiceState>,
    /// Pending engine responses to reorg messages.
    reorg_responses: FuturesUnordered<ReorgResponseFut>,
}

impl<S, Provider> EngineReorg<S, Provider> {
    /// Creates new [`EngineReorg`] stream wrapper.
    pub fn new(
        stream: S,
        provider: Provider,
        evm_config: BaseEvmConfig,
        payload_validator: BaseEngineValidator,
        frequency: usize,
        depth: usize,
    ) -> Self {
        Self {
            stream,
            provider,
            evm_config,
            payload_validator,
            frequency,
            depth,
            state: EngineReorgState::Forward,
            forkchoice_states_forwarded: 0,
            last_forkchoice_state: None,
            reorg_responses: FuturesUnordered::new(),
        }
    }
}

impl<S, Provider> Stream for EngineReorg<S, Provider>
where
    S: Stream<Item = ExecutionCommand>,
    Provider: BlockReader + StateProviderFactory + ChainSpecProvider,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            if let Poll::Ready(Some(response)) = this.reorg_responses.poll_next_unpin(cx) {
                match response {
                    Ok(Ok(status)) => {
                        debug!(target: "engine::stream::reorg", ?status, "Reorg append completed")
                    }
                    Ok(Err(error)) => {
                        error!(target: "engine::stream::reorg", %error, "Reorg append failed")
                    }
                    Err(_) => {}
                }
                continue;
            }

            if let EngineReorgState::Reorg { queue } = &mut this.state {
                match queue.pop_front() {
                    Some(msg) => return Poll::Ready(Some(msg)),
                    None => {
                        *this.forkchoice_states_forwarded = 0;
                        *this.state = EngineReorgState::Forward;
                    }
                }
            }

            let next = ready!(this.stream.poll_next_unpin(cx));
            let item = match (next, &this.last_forkchoice_state) {
                (
                    Some(ExecutionCommand::AppendPayload {
                        payload,
                        heads,
                        skip_import,
                        skip_heads,
                        tx,
                    }),
                    Some(last_forkchoice_state),
                ) if this.forkchoice_states_forwarded > this.frequency &&
                        // Only enter reorg state if new payload attaches to current head.
                        last_forkchoice_state.head_block_hash == payload.parent_hash() =>
                {
                    // Enter the reorg state.
                    // The current payload will be immediately forwarded by being in front of the
                    // queue. Then we attempt to reorg the current head by generating a payload that
                    // attaches to the head's parent and is based on the non-conflicting
                    // transactions (txs from block `n + 1` that are valid at block `n` according to
                    // consensus checks) from the current payload as well as the corresponding
                    // forkchoice state. We will rely on CL to reorg us back to canonical chain.
                    // TODO: This is an expensive blocking operation, ideally it's spawned as a task
                    // so that the stream could yield the control back.
                    let (reorg_block, encoded_bal) = match create_reorg_head(
                        this.provider,
                        this.evm_config,
                        this.payload_validator,
                        *this.depth,
                        *payload.clone(),
                    ) {
                        Ok(result) => result,
                        Err(error) => {
                            error!(target: "engine::stream::reorg", %error, "Error attempting to create reorg head");
                            // Forward the payload and attempt to create reorg on top of
                            // the next one
                            return Poll::Ready(Some(ExecutionCommand::AppendPayload {
                                payload,
                                heads,
                                skip_import,
                                skip_heads,
                                tx,
                            }));
                        }
                    };
                    let reorg_forkchoice_state = ForkchoiceState {
                        finalized_block_hash: last_forkchoice_state.finalized_block_hash,
                        safe_block_hash: last_forkchoice_state.safe_block_hash,
                        head_block_hash: reorg_block.hash(),
                    };

                    let (reorg_payload_tx, reorg_payload_rx) = oneshot::channel();
                    this.reorg_responses.push(Box::pin(reorg_payload_rx) as ReorgResponseFut);
                    let queue = VecDeque::from([
                        ExecutionCommand::AppendPayload {
                            payload,
                            heads,
                            skip_import,
                            skip_heads,
                            tx,
                        },
                        ExecutionCommand::AppendPayload {
                            payload: Box::new(BaseBuiltPayload::block_to_payload(
                                reorg_block,
                                encoded_bal,
                            )),
                            heads: reorg_forkchoice_state,
                            skip_import: false,
                            skip_heads: false,
                            tx: reorg_payload_tx,
                        },
                    ]);
                    *this.state = EngineReorgState::Reorg { queue };
                    continue;
                }
                (Some(command), _) => {
                    let heads = match &command {
                        ExecutionCommand::AppendPayload { heads, .. }
                        | ExecutionCommand::UpdateHeads { heads, .. }
                        | ExecutionCommand::StartBuilding { heads, .. } => *heads,
                    };
                    *this.last_forkchoice_state = Some(heads);
                    *this.forkchoice_states_forwarded += 1;
                    Some(command)
                }
                (item, _) => item,
            };
            return Poll::Ready(item);
        }
    }
}

#[allow(clippy::type_complexity)]
fn create_reorg_head<Provider>(
    provider: &Provider,
    evm_config: &BaseEvmConfig,
    payload_validator: &BaseEngineValidator,
    mut depth: usize,
    next_payload: base_common_types_payload::ExecutionData,
) -> Result<(SealedBlock, Option<Bytes>), base_common_types_payload::EngineRequestError>
where
    Provider: BlockReader + StateProviderFactory + ChainSpecProvider,
{
    // Ensure next payload is valid.
    let next_block = payload_validator
        .convert_payload_to_block(next_payload)
        .map_err(|error| base_common_types_payload::EngineRequestError::from(error.to_string()))?;

    // Fetch reorg target block depending on its depth and its parent.
    let mut previous_hash = next_block.parent_hash();
    let mut candidate_transactions = next_block.into_body().transactions;
    let reorg_target = 'target: {
        loop {
            let reorg_target = provider
                .block_by_hash(previous_hash)?
                .ok_or_else(|| ProviderError::HeaderNotFound(previous_hash.into()))?;
            if depth == 0 {
                break 'target reorg_target.seal_slow();
            }

            depth -= 1;
            previous_hash = reorg_target.header().parent_hash();
            candidate_transactions = reorg_target.into_body().into_transactions();
        }
    };
    let reorg_target_parent = provider
        .sealed_header_by_hash(reorg_target.header().parent_hash())?
        .ok_or_else(|| ProviderError::HeaderNotFound(reorg_target.header().parent_hash().into()))?;

    debug!(target: "engine::stream::reorg", number = reorg_target.header().number(), hash = %previous_hash, "Selected reorg target");

    // Configure state
    let has_bal = reorg_target.header().block_access_list_hash().is_some();
    let state_provider = provider.state_by_block_hash(reorg_target.header().parent_hash())?;
    let mut state = State::builder()
        .with_database_ref(&state_provider)
        .with_bundle_update()
        .with_bal_builder_if(has_bal)
        .build();

    let ctx = evm_config
        .context_for_block(&reorg_target)
        .map_err(base_common_types_payload::EngineRequestError::from)?;
    let evm = evm_config
        .evm_for_block(&mut state, &reorg_target)
        .map_err(base_common_types_payload::EngineRequestError::from)?;
    let mut builder = evm_config.create_block_builder(evm, &reorg_target_parent, ctx);

    builder.apply_pre_execution_changes()?;

    let mut cumulative_gas_used = 0;
    for tx in candidate_transactions {
        // ensure we still have capacity for this transaction
        if cumulative_gas_used + tx.gas_limit() > reorg_target.gas_limit() {
            continue;
        }

        let tx_recovered =
            tx.try_into_recovered().map_err(|_| ProviderError::SenderRecoveryError)?;
        let gas_used = match builder.execute_transaction(tx_recovered) {
            Ok(gas_used) => gas_used.tx_gas_used(),
            Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash,
                error,
            })) => {
                trace!(target: "engine::stream::reorg", hash = %hash, ?error, "Error executing transaction from next block");
                continue;
            }
            // Treat error as fatal
            Err(error) => return Err(error.into()),
        };

        cumulative_gas_used += gas_used;
    }

    let BlockBuilderOutcome { block, block_access_list, .. } =
        builder.finish(&state_provider, None)?;

    let encoded_bal: Option<Bytes> = block_access_list.map(|bal| alloy_rlp::encode(&bal).into());

    Ok((block.into_sealed_block(), encoded_bal))
}
