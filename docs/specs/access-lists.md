# Flashblock-level Access Lists (FAL)

## Abstract

This document introduces Flashblock-Level Access Lists (FAL), an adaptation of [EIP-7928](https://eips.ethereum.org/EIPS/eip-7928) Block-Level Access Lists (BAL) for chains that produce flashblocks. FAL records all accounts and storage locations accessed during flashblock execution, along with their post-execution values. Like BAL, FAL enables parallel disk reads, parallel transaction validation, and executionless state updates, but is specifically designed for the flashblock architecture used in Base.

## Motivation

FAL adapts the BAL specification for chains that produce flashblocks—incremental "mini-blocks" produced at sub-block intervals (e.g., every 200ms for a 2s canonical block time). While BAL is designed for Ethereum L1 canonical blocks, FAL must handle:

- Incremental block construction via flashblock deltas
- OP Stack-specific transaction types (L1 attributes transactions, L1→L2 deposits)
- Different fee distribution mechanisms (fee vaults instead of COINBASE)
- Absence of beacon chain features (withdrawals, beacon root, withdrawal/consolidation requests)
- OP Stack system contracts (L1Block, fee vaults, etc.)

## Key Differences from BAL

### Structural Differences

1. **Flashblock Delta Structure**: FAL operates on flashblock deltas that contain incremental changes, not complete canonical blocks
2. **Metadata Storage**: FAL hash is stored in the flashblock metadata field, not a block header field
3. **No Beacon Chain Features**: FAL omits EIP-4895 (withdrawals), EIP-4788 (beacon root), EIP-7002 (withdrawal requests), and EIP-7251 (consolidations)

### Fee Distribution

- **BAL**: Records balance changes to COINBASE address receiving transaction fees
- **FAL**: Records balance changes to OP Stack fee vaults:
  - Sequencer Fee Vault (priority fees)
  - Base Fee Vault (base fees)
  - L1 Fee Vault (L1 data fees)

### Transaction Types

- **BAL**: Only regular transactions and system contract calls
- **FAL**: Includes L1 attributes transaction (deposited tx at index 0) and L1→L2 user deposits (treated as regular transactions)

### Block Access Index Assignment

- `0`: System transaction (L1 Attributes)
- `1 to n`: Regular L2 transactions (including L1→L2 user deposits)
- `n + 1`: Post-execution system contracts (if any)

## Specification

### Flashblock Structure Modification

We introduce a new field to the flashblock metadata, `flashblock_access_list`, which contains the complete flashblock access list structure including transaction index bounds and the hash.

```rust
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FlashblocksPayloadV1 {
    pub payload_id: PayloadId,
    pub index: u64,
    pub base: Option<ExecutionPayloadBaseV1>,
    pub diff: ExecutionPayloadFlashblockDeltaV1,
    pub metadata: Value, // Contains "flashblock_access_list"
}

// The flashblock_access_list field in metadata contains:
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FlashblockAccessList {
    pub min_tx_index: u64,      // Inclusive starting transaction index in the overall block
    pub max_tx_index: u64,      // Exclusive ending transaction index in the overall block
    pub account_changes: Vec<AccountChanges>,  // List of all account changes
    pub fal_hash: B256,         // Keccak-256 hash of RLP-encoded account_changes
}
```

**Transaction Index Bounds:**
- `min_tx_index` (inclusive): The starting transaction index in the overall block
- `max_tx_index` (exclusive): The ending transaction index in the overall block

These bounds allow each flashblock to maintain its position within the overall block's transaction sequence.

**Example:** If a block has 15 transactions split across 3 flashblocks:
- Flashblock 0: `min_tx_index=0, max_tx_index=5` (contains transactions 0-4)
- Flashblock 1: `min_tx_index=5, max_tx_index=10` (contains transactions 5-9)
- Flashblock 2: `min_tx_index=10, max_tx_index=15` (contains transactions 10-14)

When no state changes are present, `account_changes` is an empty list and `fal_hash` is the hash of an empty RLP list: `0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347`, i.e., `keccak256(rlp.encode([]))`.

### RLP Data Structures

FAL uses the same RLP encoding as BAL, following the pattern: `address -> field -> block_access_index -> change`.

```python
# Type aliases for RLP encoding (identical to BAL)
Address = bytes  # 20-byte Ethereum address
StorageKey = bytes  # 32-byte storage slot key
StorageValue = bytes  # 32-byte storage value
CodeData = bytes  # Variable-length contract bytecode
BlockAccessIndex = uint16  # Block access index (0 for pre-execution, 1..n for transactions, n+1 for post-execution)
Balance = uint256  # Post-transaction balance in wei
Nonce = uint64  # Account nonce

# Constants (adapted for OP Stack)
MAX_TXS = 30_000
MAX_SLOTS = 300_000
MAX_ACCOUNTS = 300_000
MAX_CODE_SIZE = 24_576
MAX_CODE_CHANGES = 1

# Core change structures (identical to BAL)
StorageChange = [BlockAccessIndex, StorageValue]
BalanceChange = [BlockAccessIndex, Balance]
NonceChange = [BlockAccessIndex, Nonce]
CodeChange = [BlockAccessIndex, CodeData]
SlotChanges = [StorageKey, List[StorageChange]]

# AccountChanges: [address, storage_changes, storage_reads, balance_changes, nonce_changes, code_changes]
AccountChanges = [
    Address,
    List[SlotChanges],          # storage_changes
    List[StorageKey],           # storage_reads
    List[BalanceChange],        # balance_changes
    List[NonceChange],          # nonce_changes
    List[CodeChange]            # code_changes
]

# FlashblockAccessList: List of AccountChanges
FlashblockAccessList = List[AccountChanges]
```

### Scope and Inclusion

**`FlashblockAccessList`** is the set of all addresses accessed during flashblock execution.

It **MUST** include:

- Addresses with state changes (storage, balance, nonce, or code)
- Addresses accessed without state changes, including:
  - Targets of `BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH` opcodes
  - Targets of `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL` (even if they revert)
  - Target addresses of `CREATE`/`CREATE2` (even when creation fails)
  - Transaction sender and recipient addresses (even for zero-value transfers)
  - **Fee vault addresses** (Sequencer Fee Vault, Base Fee Vault, L1 Fee Vault) when receiving fees
  - Beneficiary addresses for `SELFDESTRUCT`
  - System contract addresses accessed during pre/post-execution
  - **L1→L2 deposit recipient addresses**
  - Precompiled contracts when called or accessed
  - **L1Block contract** when accessed by L1 attributes transaction

Addresses with no state changes **MUST** still be present with empty change lists.

Entries from an EIP-2930 access list **MUST NOT** be included automatically. Only addresses and storage slots that are actually touched or changed during execution are recorded.

### Ordering and Determinism

The following ordering rules **MUST** apply:

- **Addresses:** lexicographic (bytewise)
- **Storage keys:** lexicographic within each account
- **Block access indices:** ascending within each change list

### BlockAccessIndex Assignment

`BlockAccessIndex` values **MUST** be assigned as follows:

- `0` for **pre-execution** system contract calls and **L1 attributes transaction** (only in the first flashblock where `min_tx_index = 0`)
- `min_tx_index … max_tx_index - 1` for transactions in this flashblock (including L1→L2 user deposits and regular L2 transactions)
- `n + 1` for **post-execution** system contract calls (if any, only in the final flashblock)

**Important:** The `block_access_index` for transactions uses the **overall block transaction index**, not the flashblock-local index. This allows FALs from multiple flashblocks to be combined while maintaining correct transaction ordering.

### Recording Semantics by Change Type

#### Storage

- **Writes include:**
  - Any value change (post-value ≠ pre-value)
  - **Zeroing** a slot (pre-value exists, post-value is zero)

- **Reads include:**
  - Slots accessed via `SLOAD` that are not written
  - Slots written with unchanged values (i.e., `SSTORE` where post-value equals pre-value, also known as "no-op writes")

Note: Implementations MUST check the pre-transaction value to correctly distinguish between actual writes and no-op writes.

#### Balance (`balance_changes`)

Record **post-transaction** balances (`uint256`) for:

- Transaction **senders** (gas + value + L1 data fee)
- Transaction **recipients** (only if `value > 0`)
- **Fee vaults** (Sequencer Fee Vault, Base Fee Vault, L1 Fee Vault) receiving fees after each transaction
- **SELFDESTRUCT/SENDALL** beneficiaries
- **L1→L2 deposit recipients**

**Zero-value transfers:** **MUST NOT** be recorded in `balance_changes`, but the corresponding addresses **MUST** still be included with empty `AccountChanges`.

#### Code

Track **post-transaction runtime bytecode** for deployed or modified contracts, and **delegation indicators** for successful delegations as defined in EIP-7702.

#### Nonce

Record **post-transaction nonces** for:

- EOA senders
- Contracts that performed a successful `CREATE` or `CREATE2`
- Deployed contracts
- EIP-7702 authorities

### OP Stack-Specific Edge Cases

#### Fee Vaults

**Sequencer Fee Vault** (0x4200000000000000000000000000000000000011):
- Records balance changes when priority fees are collected
- Balance updated after each transaction that pays priority fees

**Base Fee Vault** (0x4200000000000000000000000000000000000019):
- Records balance changes when base fees are collected
- Balance updated after each transaction

**L1 Fee Vault** (0x420000000000000000000000000000000000001a):
- Records balance changes when L1 data fees are collected
- Balance updated after each transaction that pays L1 fees

#### L1 Attributes Transaction

The **L1 attributes transaction** (deposited transaction at index 0):
- Uses `block_access_index = 0`
- Updates the L1Block contract (0x4200000000000000000000000000000000000015)
- Records storage changes to L1Block contract slots:
  - `basefee` (slot 1)
  - `blobBaseFee` (slot 5)
  - `hash` (slot 6)
  - `number` (slot 0)
  - `timestamp` (slot 2)
  - `sequenceNumber` (slot 4)
  - `batcherHash` (slot 3)

#### L1→L2 User Deposits

**User deposits from L1** (e.g., via OptimismPortal):
- Treated as regular transactions with `block_access_index = 1..n`
- Record sender (L1 address) and recipient (L2 address) with balance changes
- Include gas fees paid to fee vaults

#### EIP-2935 (Block Hash Storage)

Record system contract storage diffs of the **single** updated storage slot in the ring buffer.

**OP Stack Note:** Block hash storage may use the same [EIP-2935](https://specs.optimism.io/protocol/isthmus/derivation.html#eip-2935-contract-deployment) mechanism or a modified version. Record whatever storage changes actually occur. See [OP Stack Specification](https://eips.ethereum.org/EIPS/eip-2935?utm_source=chatgpt.com) for details on block hash storage implementation.

### Edge Cases (General, inherited from BAL)

- **Precompiled contracts:** Precompiles **MUST** be included when accessed. If a precompile receives value, it is recorded with a balance change. Otherwise, it is included with empty change lists.
- **SENDALL:** For positive-value selfdestructs, the sender and beneficiary are recorded with a balance change.
- **SELFDESTRUCT (in-transaction):** Accounts destroyed within a transaction **MUST** be included in `AccountChanges` without nonce or code changes. However, if the account had a positive balance pre-transaction, the balance change to zero **MUST** be recorded. Storage keys within the self-destructed contracts that were modified or read **MUST** be included as a `storage_read`.
- **Accessed but unchanged:** Include the address with empty changes (e.g., targets of `EXTCODEHASH`, `EXTCODESIZE`, `BALANCE`, `STATICCALL`, etc.).
- **Zero-value transfers:** Include the address; omit from `balance_changes`.
- **Gas refunds:** Record the **final** balance of the sender after each transaction.
- **Fee vault payments:** Record the **final** balance of each fee vault after each transaction that contributes fees.
- **Exceptional halts:** Record the **final** nonce and balance of the sender, and the **final** balance of fee vaults after each transaction. State changes from the reverted call are discarded, but all accessed addresses **MUST** be included. If no changes remain, addresses are included with empty lists; if storage was read, the corresponding keys **MUST** appear in `storage_reads`.
- **Pre-execution system contract calls:** All state changes **MUST** use `block_access_index = 0`.
- **Post-execution system contract calls:** All state changes **MUST** use `block_access_index = len(transactions) + 1`.
- **EIP-7702 Delegations:** The authority address **MUST** be included with the nonce and code changes after any successful delegation set, reset, or update, and **MUST** also be included with an empty change set if authorization fails due to an invalid nonce. The delegation target **MUST NOT** be included during delegation creation and **MUST** be included when loaded as a call target under authority execution.

### Validation

The state transition function must validate that the provided FAL matches the actual state accesses:

```python
def validate_flashblock(flashblock):
    # 1. Extract FAL from metadata
    import rlp
    fal = flashblock.metadata['flashblock_access_list']
    min_tx_index = fal['min_tx_index']
    max_tx_index = fal['max_tx_index']
    provided_account_changes = fal['account_changes']
    provided_fal_hash = fal['fal_hash']

    # 2. Verify provided hash matches account_changes
    computed_hash = keccak256(rlp.encode(provided_account_changes))
    assert computed_hash == provided_fal_hash

    # 3. Execute flashblock and collect actual accesses
    actual_account_changes = execute_and_collect_accesses(flashblock, min_tx_index, max_tx_index)

    # 4. Verify actual execution matches provided account_changes
    actual_fal_hash = keccak256(rlp.encode(actual_account_changes))
    assert actual_fal_hash == provided_fal_hash

def execute_and_collect_accesses(flashblock, min_tx_index, max_tx_index):
    """Execute flashblock and collect all state accesses into FAL format

    Args:
        flashblock: The flashblock to execute
        min_tx_index: Starting transaction index (inclusive) in the overall block
        max_tx_index: Ending transaction index (exclusive) in the overall block
    """
    accesses = {}

    # Pre-execution: L1 attributes transaction (block_access_index = 0)
    # Only include for first flashblock (min_tx_index == 0)
    if min_tx_index == 0:
        track_l1_attributes_tx(flashblock, accesses, block_access_index=0)
        track_system_contracts_pre(flashblock, accesses, block_access_index=0)

    # Execute transactions (block_access_index = min_tx_index..max_tx_index)
    # This includes both L1→L2 deposits and regular L2 transactions
    for i, tx in enumerate(flashblock.diff.transactions):
        tx_index = min_tx_index + i
        execute_transaction(tx)
        track_state_changes(tx, accesses, block_access_index=tx_index)
        track_fee_vault_changes(tx, accesses, block_access_index=tx_index)

    # Post-execution system contracts
    # Only include for last flashblock (would need to be indicated separately)
    # For now, omitted as flashblocks are incremental

    # Convert to FAL format and sort
    return build_fal(accesses)

def track_state_changes(tx, accesses, block_access_index):
    """Track all state changes from a transaction"""
    for addr in get_touched_addresses(tx):
        if addr not in accesses:
            accesses[addr] = {
                'storage_writes': {},  # slot -> [(index, value)]
                'storage_reads': set(),
                'balance_changes': [],
                'nonce_changes': [],
                'code_changes': []
            }

        # Track storage changes
        for slot, value in get_storage_writes(addr).items():
            if slot not in accesses[addr]['storage_writes']:
                accesses[addr]['storage_writes'][slot] = []
            accesses[addr]['storage_writes'][slot].append((block_access_index, value))

        # Track reads (slots accessed but not written)
        for slot in get_storage_reads(addr):
            if slot not in accesses[addr]['storage_writes']:
                accesses[addr]['storage_reads'].add(slot)

        # Track balance, nonce, code changes
        if balance_changed(addr):
            accesses[addr]['balance_changes'].append((block_access_index, get_balance(addr)))
        if nonce_changed(addr):
            accesses[addr]['nonce_changes'].append((block_access_index, get_nonce(addr)))
        if code_changed(addr):
            accesses[addr]['code_changes'].append((block_access_index, get_code(addr)))

def track_fee_vault_changes(tx, accesses, block_access_index):
    """Track OP Stack fee vault balance changes after each transaction"""
    # Sequencer Fee Vault (priority fees)
    SEQUENCER_FEE_VAULT = 0x4200000000000000000000000000000000000011
    # Base Fee Vault (base fees)
    BASE_FEE_VAULT = 0x4200000000000000000000000000000000000019
    # L1 Fee Vault (L1 data fees)
    L1_FEE_VAULT = 0x420000000000000000000000000000000000001a

    for vault in [SEQUENCER_FEE_VAULT, BASE_FEE_VAULT, L1_FEE_VAULT]:
        if vault not in accesses:
            accesses[vault] = {
                'storage_writes': {},
                'storage_reads': set(),
                'balance_changes': [],
                'nonce_changes': [],
                'code_changes': []
            }

        # Record vault balance after transaction if it changed
        if balance_changed(vault):
            accesses[vault]['balance_changes'].append((block_access_index, get_balance(vault)))

def track_l1_attributes_tx(flashblock, accesses, block_access_index):
    """Track L1 attributes transaction (deposited tx at index 0)"""
    L1_BLOCK_CONTRACT = 0x4200000000000000000000000000000000000015

    if L1_BLOCK_CONTRACT not in accesses:
        accesses[L1_BLOCK_CONTRACT] = {
            'storage_writes': {},
            'storage_reads': set(),
            'balance_changes': [],
            'nonce_changes': [],
            'code_changes': []
        }

    # Track storage updates to L1Block contract
    # Slots: number(0), basefee(1), timestamp(2), batcherHash(3),
    #        sequenceNumber(4), blobBaseFee(5), hash(6)
    for slot in [0, 1, 2, 3, 4, 5, 6]:
        if slot_changed(L1_BLOCK_CONTRACT, slot):
            if slot not in accesses[L1_BLOCK_CONTRACT]['storage_writes']:
                accesses[L1_BLOCK_CONTRACT]['storage_writes'][slot] = []
            value = get_storage(L1_BLOCK_CONTRACT, slot)
            accesses[L1_BLOCK_CONTRACT]['storage_writes'][slot].append((block_access_index, value))

def build_fal(accesses):
    """Convert collected accesses to FAL format"""
    fal = []
    for addr in sorted(accesses.keys()):  # Sort addresses lexicographically
        data = accesses[addr]

        # Format storage changes: [slot, [[index, value], ...]]
        storage_changes = [[slot, sorted(changes)]
                          for slot, changes in sorted(data['storage_writes'].items())]

        # Account entry: [address, storage_changes, reads, balance_changes, nonce_changes, code_changes]
        fal.append([
            addr,
            storage_changes,
            sorted(list(data['storage_reads'])),
            sorted(data['balance_changes']),
            sorted(data['nonce_changes']),
            sorted(data['code_changes'])
        ])

    return fal
```

The FAL SHOULD be complete and accurate. Being out of protocol, best-effort construction of FALs is attempted for the performance benefits to validator nodes. Real Flashblocks and canonical blocks data should still be considered authoritative.

Clients MAY validate by comparing execution-gathered accesses with the FAL.

### Concrete Example

Example flashblock on OP Stack:

**Pre-execution (block_access_index = 0):**
- L1 attributes transaction updates L1Block contract (0x4200000000000000000000000000000000000015)
- EIP-2935 block hash storage at 0x0000F90827F1C53a10cb7A02335B175320002935

**Transactions:**
1. Alice (0xaaaa...) sends 1 ETH to Bob (0xbbbb...)
2. Charlie (0xcccc...) calls factory (0xffff...) deploying contract at 0xdddd...
3. L1→L2 deposit: Dave (0xdave...) receives 10 ETH from L1

**Post-execution (block_access_index = 4):**
- None in this example

Resulting FAL (RLP structure):

```python
[
    # Addresses are sorted lexicographically
    [ # AccountChanges for 0x0000F90827F1C53a10cb7A02335B175320002935 (Block hash contract)
        0x0000F90827F1C53a10cb7A02335B175320002935,
        [ # storage_changes
            [b'\\x00...\\x0f\\xa0', [[0, b'...']]]  # slot, [[block_access_index, parent_hash]]
        ],
        [],  # storage_reads
        [],  # balance_changes
        [],  # nonce_changes
        []   # code_changes
    ],
    [ # AccountChanges for 0x4200000000000000000000000000000000000011 (Sequencer Fee Vault)
        0x4200000000000000000000000000000000000011,
        [],  # storage_changes
        [],  # storage_reads
        [[1, 0x...fee1], [2, 0x...fee2], [3, 0x...fee3]],  # balance_changes: after each tx
        [],  # nonce_changes
        []   # code_changes
    ],
    [ # AccountChanges for 0x4200000000000000000000000000000000000015 (L1Block contract)
        0x4200000000000000000000000000000000000015,
        [ # storage_changes from L1 attributes tx
            [b'\\x00...\\x00', [[0, b'...']]],  # number
            [b'\\x00...\\x01', [[0, b'...']]],  # basefee
            [b'\\x00...\\x02', [[0, b'...']]],  # timestamp
            [b'\\x00...\\x03', [[0, b'...']]],  # batcherHash
            [b'\\x00...\\x04', [[0, b'...']]],  # sequenceNumber
            [b'\\x00...\\x05', [[0, b'...']]],  # blobBaseFee
            [b'\\x00...\\x06', [[0, b'...']]]   # hash
        ],
        [],  # storage_reads
        [],  # balance_changes
        [],  # nonce_changes
        []   # code_changes
    ],
    [ # AccountChanges for 0x4200000000000000000000000000000000000019 (Base Fee Vault)
        0x4200000000000000000000000000000000000019,
        [],  # storage_changes
        [],  # storage_reads
        [[1, 0x...base1], [2, 0x...base2], [3, 0x...base3]],  # balance_changes: after each tx
        [],  # nonce_changes
        []   # code_changes
    ],
    [ # AccountChanges for 0x420000000000000000000000000000000000001a (L1 Fee Vault)
        0x420000000000000000000000000000000000001a,
        [],  # storage_changes
        [],  # storage_reads
        [[1, 0x...l1fee1], [2, 0x...l1fee2], [3, 0x...l1fee3]],  # balance_changes: after each tx
        [],  # nonce_changes
        []   # code_changes
    ],
    [ # AccountChanges for 0xaaaa... (Alice - sender tx 1)
        0xaaaa...,
        [],  # storage_changes
        [],  # storage_reads
        [[1, 0x...29a241a]],  # balance_changes: [[block_access_index, post_balance]]
        [[1, 10]],  # nonce_changes: [[block_access_index, new_nonce]]
        []  # code_changes
    ],
    [ # AccountChanges for 0xbbbb... (Bob - recipient tx 1)
        0xbbbb...,
        [],  # storage_changes
        [],  # storage_reads
        [[1, 0x...b9aca00]],  # balance_changes: +1 ETH
        [],  # nonce_changes
        []   # code_changes
    ],
    [ # AccountChanges for 0xcccc... (Charlie - sender tx 2)
        0xcccc...,
        [],  # storage_changes
        [],  # storage_reads
        [[2, 0x...bc16d67]],  # balance_changes: after gas
        [[2, 5]],  # nonce_changes
        []  # code_changes
    ],
    [ # AccountChanges for 0xdave... (Dave - L1→L2 deposit recipient tx 3)
        0xdave...,
        [],  # storage_changes
        [],  # storage_reads
        [[3, 0x...8ac7230]],  # balance_changes: +10 ETH from L1
        [],  # nonce_changes
        []   # code_changes
    ],
    [ # AccountChanges for 0xdddd... (Deployed contract)
        0xdddd...,
        [],  # storage_changes
        [],  # storage_reads
        [],  # balance_changes
        [[2, 1]],  # nonce_changes: new contract nonce
        [[2, b'\\x60\\x80\\x60\\x40...']]  # code_changes: deployed bytecode
    ],
    [ # AccountChanges for 0xffff... (Factory contract)
        0xffff...,
        [ # storage_changes
            [b'\\x00...\\x01', [[2, b'\\x00...\\xdd\\xdd...']]]  # slot 1, deployed address
        ],
        [],  # storage_reads
        [],  # balance_changes
        [[2, 5]],  # nonce_changes: after CREATE
        []  # code_changes
    ]
]
```

RLP-encoded and compressed: ~500-600 bytes (slightly larger than BAL due to L1Block contract updates).

## Rationale

### FAL Design Choices

1. **Flashblock Compatibility**: FAL is designed for incremental flashblock deltas while maintaining compatibility with BAL's core structure

2. **OP Stack Specificity**: FAL handles OP Stack-specific features:
   - Multiple fee vaults instead of single COINBASE
   - L1 attributes transaction for block context
   - L1→L2 deposits treated as regular transactions
   - L1Block contract updates

3. **Omitted Features**: Beacon chain features (EIP-4895, 4788, 7002, 7251) are omitted as they don't exist in OP Stack

4. **Size Overhead**: Expected flashblock access list size is similar to BAL (~40-50 KiB compressed on average) with additional overhead from:
   - L1Block contract updates (~200 bytes per flashblock)
   - Three fee vault balance updates per transaction vs one COINBASE update

5. **Parallel Execution**: Like BAL, FAL enables:
   - Parallel disk reads across transactions
   - Parallel transaction validation
   - State reconstruction without execution

## Considerations

### Flashblock Size

Increased flashblock size impacts propagation. Average overhead can be reasonable for 200ms flashblock intervals and enables significant performance gains through parallelization.

## References

- [EIP-7928: Block-Level Access Lists](https://eips.ethereum.org/EIPS/eip-7928) - The original BAL specification for Ethereum L1
- [EIP-2930: Optional Access Lists](https://eips.ethereum.org/EIPS/eip-2930) - Transaction-level access lists
- [OP Stack Specification](https://specs.optimism.io/) - OP Stack technical specifications

## Copyright

Copyright and related rights waived via CC0.
