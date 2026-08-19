# ExEx Internal Transactions — Design Document (v2.5.0)

## Background

Requirements document: `docs/spec/exex-internal-txs.md`

This design ports the ExEx call traces feature to reth v2.5.0. The core concept: inject a
`TracingInspector` during Engine path block execution, collect per-transaction call frames, and
propagate them through `ExecutedBlock` → `Chain` → `ExExNotification`.

v2.5.0 changes vs v2.4.0 that affect this feature:
1. `reth-chain-state` removed the `rayon` feature — dependency declaration simplified
2. `payload_validator.rs` has new `terminate_caching` and `LazyHashedPostState` logic after execution
3. `BlockExecutionOutput` is now wrapped in `Arc::new(output)` after execution
4. `hashed_post_state` mock in tests returns `ProviderResult<HashedPostState>` instead of `HashedPostState`
5. `reth-storage-overlay` extracted as a separate crate (was in `reth-storage`)

None of these affect our feature's interfaces or data flow.

## Goal

- Satisfy FR-1 through FR-7 and NFR-1 (Should), NFR-2, NFR-3, NFR-4
- Minimal change surface to v2.5.0 code
- Pipeline path, `ExExNotification` struct, and public `Chain` API remain unchanged
- Existing `execute_transactions` function remains unmodified

## Module Overview

```
┌─────────────────────────────────────────────────────────────┐
│ crates/engine/tree/src/tree/payload_validator.rs            │
│                                                             │
│  execute_block()                                            │
│    ├── evm_with_env_and_inspector(TracingInspector) ──┐     │
│    ├── execute_transactions_traced() ◄── NEW          │     │
│    │     └── per-tx: geth_call_traces() + fuse()      │     │
│    └── returns (output, senders, rx, bal, call_traces) │     │
│                                                             │
│  execute_block_bal()                                        │
│    └── returns (..., call_traces: None)                     │
│                                                             │
│  validate_block_with_state()                                │
│    └── executed_block.call_traces = call_traces             │
├─────────────────────────────────────────────────────────────┤
│ crates/chain-state/src/in_memory.rs                         │
│                                                             │
│  ExecutedBlock { ..., call_traces: Option<Vec<CallFrame>> } │
│  blocks_to_chain(blocks) → Chain                            │
├─────────────────────────────────────────────────────────────┤
│ crates/evm/execution-types/src/chain.rs                     │
│                                                             │
│  Chain { ..., call_traces: Option<BTreeMap<BlockNumber,     │
│                                   Vec<CallFrame>>> }        │
│  Chain::with_call_traces() / Chain::call_traces()           │
├─────────────────────────────────────────────────────────────┤
│ Notification delivery (NO CHANGES):                         │
│  CanonStateNotification contains Arc<Chain<N>>              │
│  ExExNotification::From<CanonStateNotification> passes      │
│  Chain through unchanged — call_traces is available to      │
│  ExEx subscribers via chain.call_traces()                   │
└─────────────────────────────────────────────────────────────┘
```

## Interface Definitions

### New: `Chain::call_traces` field and methods

```rust
// crates/evm/execution-types/src/chain.rs

pub struct Chain<N: NodePrimitives = EthPrimitives> {
    blocks: BTreeMap<BlockNumber, Arc<RecoveredBlock<N::Block>>>,
    execution_outcome: ExecutionOutcome<N::Receipt>,
    trie_data: BTreeMap<BlockNumber, LazyTrieData>,
    #[cfg(feature = "traces")]
    #[cfg_attr(feature = "serde", serde(skip))]
    call_traces: Option<BTreeMap<BlockNumber, Vec<CallFrame>>>,
}

impl<N: NodePrimitives> Chain<N> {
    #[cfg(feature = "traces")]
    pub fn with_call_traces(mut self, traces: BTreeMap<BlockNumber, Vec<CallFrame>>) -> Self;

    #[cfg(feature = "traces")]
    pub fn call_traces(&self) -> Option<&BTreeMap<BlockNumber, Vec<CallFrame>>>;
}
```

### New: `ExecutedBlock::call_traces` field

```rust
// crates/chain-state/src/in_memory.rs

pub struct ExecutedBlock<N: NodePrimitives = EthPrimitives> {
    pub recovered_block: Arc<RecoveredBlock<N::Block>>,
    pub execution_output: Arc<BlockExecutionOutput<N::Receipt>>,
    pub trie_data: LazyTrieData,
    #[cfg(feature = "traces")]
    pub call_traces: Option<Vec<CallFrame>>,
}
```

### Modified: `execute_block` return type

```rust
// Before (v2.5.0 original):
fn execute_block(...) -> Result<(
    BlockExecutionOutput<N::Receipt>,
    Vec<Address>,
    ReceiptRootReceiver,
    Option<BlockAccessList>,
), InsertBlockErrorKind>

// After:
fn execute_block(...) -> Result<(
    BlockExecutionOutput<N::Receipt>,
    Vec<Address>,
    ReceiptRootReceiver,
    Option<BlockAccessList>,
    Option<Vec<CallFrame>>,  // NEW
), InsertBlockErrorKind>
```

### Modified: `execute_block_bal` return type

```rust
// Same 5-tuple shape, always returns None for call_traces
fn execute_block_bal(...) -> Result<(
    BlockExecutionOutput<N::Receipt>,
    Vec<Address>,
    ReceiptRootReceiver,
    Option<BlockAccessList>,
    Option<Vec<CallFrame>>,  // Always None
), InsertBlockErrorKind>
```

### New: `execute_transactions_traced` function

```rust
fn execute_transactions_traced<'a, E, Tx, InnerTx, Err, DB>(
    &self,
    mut executor: E,
    transaction_count: usize,
    transactions: impl Iterator<Item = Result<Tx, Err>>,
    receipt_tx: &crossbeam_channel::Sender<IndexedReceipt<N::Receipt>>,
    executed_tx_index: &AtomicUsize,
    has_bal: bool,
) -> Result<(E, Vec<Address>, Vec<CallFrame>), BlockExecutionError>
where
    E: BlockExecutor<
        Receipt = N::Receipt,
        Evm: alloy_evm::Evm<DB = &'a mut State<DB>, Inspector = TracingInspector>,
    >,
    Tx: alloy_evm::block::ExecutableTx<E> + alloy_evm::RecoveredTx<InnerTx>,
    InnerTx: TxHashRef,
    DB: revm::Database + 'a,
    Err: core::error::Error + Send + Sync + 'static;
```

## Data Structures

### CallFrame (from `alloy-rpc-types-trace`)

External type, not modified. Same type used by RPC `debug_traceTransaction?tracer=callTracer`.
Key fields:
- `typ`: call type (CALL, STATICCALL, DELEGATECALL, CREATE, etc.)
- `from`, `to`: addresses
- `value`: transfer value
- `gas`, `gas_used`: gas accounting
- `input`, `output`: calldata bytes
- `calls`: nested call tree (recursive)

### Trace content control

`TracingInspectorConfig::none()` disables opcode/memory/stack recording but still records the
call tree structure. `CallConfig::default()` passed to `geth_call_traces()` includes `input`
and `output` fields, matching Geth's callTracer default behavior.

### Data ownership

| Data | Owner (write) | Readers |
|------|---------------|---------|
| `ExecutedBlock.call_traces` | `payload_validator.rs` (after execution) | `in_memory.rs` (blocks_to_chain) |
| `Chain.call_traces` | `in_memory.rs` (blocks_to_chain) | ExEx subscribers (via notification) |

### Data format and boundaries

| Field | Type | Nullable | Default | Range |
|-------|------|----------|---------|-------|
| `ExecutedBlock.call_traces` | `Option<Vec<CallFrame>>` | Yes | `None` | 0..N frames (one per tx) |
| `Chain.call_traces` | `Option<BTreeMap<BlockNumber, Vec<CallFrame>>>` | Yes | `None` | Keyed by block numbers in chain |

## Dependency Graph

```
reth-engine-tree
  ├── reth-chain-state (features: ["traces"])           ← add "traces"
  ├── alloy-rpc-types-trace (new dep)
  └── revm-inspectors (new dep)

reth-chain-state
  └── reth-execution-types (features: ["traces"])       ← propagate

reth-execution-types
  └── alloy-rpc-types-trace (new dep, optional under "traces" feature)
```

Note: v2.5.0 removed the `rayon` feature from `reth-chain-state`. The `reth-engine-tree`
dependency on `reth-chain-state` is now declared as `reth-chain-state.workspace = true`
(no features). We add `features = ["traces"]` to this declaration.

## Implementation Plan

### Step 1: Data model — `reth-execution-types` (chain.rs)

1. Add `alloy-rpc-types-trace` as optional dependency under `traces` feature in Cargo.toml
2. Add `call_traces` field to `Chain` struct (cfg-gated, serde skip)
3. Add `with_call_traces()` and `call_traces()` methods
4. Update `Default` impl and `new()` to initialize `call_traces: None`
5. In `append_chain`: add `debug_assert!(other.call_traces.is_none())`
6. Update `serde_bincode_compat` module: add skip field and update From impls

### Step 2: Data model — `reth-chain-state` (in_memory.rs)

1. Add `traces` feature to Cargo.toml (propagates to `reth-execution-types/traces`)
2. Add `alloy-rpc-types-trace` as optional dependency under `traces` feature
3. Add `call_traces` field to `ExecutedBlock` (cfg-gated, `pub`)
4. Update constructors (`Default`, `new()`, `with_deferred_trie_data()`) — all init `None`
5. In `blocks_to_chain`: append trace collection at the end (cfg-gated block)

### Step 3: Execution — `reth-engine-tree` (payload_validator.rs)

1. Add `alloy-rpc-types-trace` and `revm-inspectors` to Cargo.toml
2. Enable `reth-chain-state/traces` feature: change `reth-chain-state.workspace = true` to
   `reth-chain-state = { workspace = true, features = ["traces"] }`
3. In `execute_block`:
   - Replace `evm_with_env` with `evm_with_env_and_inspector(..., TracingInspector::new(...))`
   - Call `execute_transactions_traced` instead of `execute_transactions`
   - Add `Option<Vec<CallFrame>>` to return tuple (5th element)
4. In `execute_block_bal`: add `None` for call_traces in return tuple (5th element)
5. In `validate_block_with_state`: destructure 5-tuple, assign `executed_block.call_traces`
6. Implement `execute_transactions_traced`:
   - Same structure as v2.5.0's `execute_transactions`
   - After `apply_pre_execution_changes`: `fuse()` to discard system call traces
   - After each `execute_transaction`: extract `gas_used` via `GasOutput`, call
     `geth_builder().geth_call_traces(Default::default(), gas_used)`, push, `fuse()`
   - Return `(executor, senders, tx_traces)`
7. Mark original `execute_transactions` as `#[allow(dead_code)]` (kept for upstream merge ease)

### Step 4: Documentation

- `docs/spec/exex-internal-txs.md` — requirements (already done)
- `docs/design/exex-internal-txs.md` — this file

## Alternative Approaches

### Alternative: Modify `execute_transactions` with inspector parameter

**Rejected**: Adds type complexity to an already heavily generic function; risks breaking
non-tracing path; borrow-checker friction with `&mut executor` + closure capturing trace vec.

### Alternative: Inline transaction loop in `execute_block`

**Rejected**: Reverses v2.5.0 refactoring; makes `execute_block` too long; duplicates future
upstream changes.

### Chosen: New `execute_transactions_traced` function

Same rationale as v2.3.0/v2.4.0: zero risk to existing path, clean type bounds, matches
project pattern. Only reasonable approach given NFR-3 constraint.

## Concurrency Analysis

- **TracingInspector**: owned solely by the EVM instance within `execute_block`; no shared access
- **`ExecutedBlock.call_traces`**: written once after execution, then only read through `Arc`
- **`Chain.call_traces`**: built in `blocks_to_chain` (single-threaded), then shared via
  `Arc<Chain>` (read-only)
- **BAL parallel workers**: each has its own EVM; no tracing inspector injected
- **`terminate_caching`** (v2.5.0 new): takes `output.clone()` before we extract traces — no conflict

No new concurrent mutable state is introduced.

## Error Handling

- `TracingInspector` cannot fail — it passively records calls. No new error paths introduced.
- If `executor.finish()` fails after traces are collected, traces are discarded along with the
  entire execution result (existing behavior, no special handling needed).

## Known Limitations and Risks

1. **~50 lines of structural overlap** between `execute_transactions` and
   `execute_transactions_traced`. Cross-reference comments in both functions.
2. **BAL path returns no traces** — documented, consistent with pipeline blocks.
3. **Performance**: `TracingInspectorConfig::none()` still records call tree. Expected < 20%
   overhead on average blocks. Not benchmarked this iteration (NFR-1 is Should).
4. **Memory**: No trace size cap. Typical mainnet blocks: 1-5MB. Pathological: up to
   50-100MB. With 128 in-memory blocks, worst-case several GB. Deferred to future iteration.
5. **Feature gate**: `#[cfg(feature = "traces")]` avoids pulling heavy deps for non-ExEx nodes.
6. **WAL recovery loses traces**: Blocks from WAL have `call_traces = None` (serde skip).
7. **Reorg old chain includes traces if present**: Old chain blocks that were previously
   executed via Engine path retain their traces. ExEx consumers distinguish via
   `ChainReorged { old, new }` variant.

## Test Plan

### Unit Tests — `reth-execution-types` (chain.rs)

| # | Case | Input | Expected |
|---|------|-------|----------|
| T1 | `test_chain_default_no_traces` | `Chain::default()` | `call_traces()` returns `None` |
| T2 | `test_chain_with_call_traces` | Chain + BTreeMap with 2 blocks | `call_traces()` returns `Some` with both |
| T3 | `test_chain_new_no_traces` | `Chain::new(blocks, outcome, trie)` | `call_traces()` returns `None` |
| T4 | `test_chain_with_call_traces_replace` | Chain with traces, call `with_call_traces` again | Old traces replaced |
| T5 | `test_chain_bincode_roundtrip_skips_traces` | Serialize/deserialize | `call_traces()` is `None` after roundtrip |

### Unit Tests — `reth-chain-state` (in_memory.rs)

| # | Case | Input | Expected |
|---|------|-------|----------|
| T6 | `test_blocks_to_chain_injects_traces` | Blocks with `call_traces = Some(...)` | Chain has matching traces |
| T7 | `test_blocks_to_chain_no_traces_when_none` | Blocks with `call_traces = None` | Chain `call_traces()` is `None` |
| T8 | `test_blocks_to_chain_partial_traces` | Mix of Some/None blocks | Chain has traces only for Some blocks |
| T9 | `test_to_chain_notification_commit_includes_traces` | Commit with traced blocks | `new.call_traces()` is `Some` |
| T10 | `test_to_chain_notification_reorg_traces` | Reorg: new(traced) + old(no traces) | `new` has traces, `old` is `None` |
| T11 | `test_set_call_traces_field` | Set traces then check | Behaves correctly |
| T12 | `test_blocks_to_chain_empty_block_traces` | Block with `call_traces = Some(vec![])` (0 user txs) | Chain has entry for that block number with empty vec (AC-8) |

### Unit Tests — `reth-engine-tree` (payload_validator.rs)

These tests validate the traced execution logic. Due to the complexity of setting up a full
EVM + executor + state in unit tests, the primary verification will be via compilation
(type system guarantees correctness of inspector wiring) and integration with the existing
test infrastructure.

| # | Case | Description | Expected |
|---|------|-------------|----------|
| T13 | Type-level verification | `execute_transactions_traced` compiles with `Inspector = TracingInspector` bound | Compiles |
| T14 | `execute_block` returns 5-tuple | Verified by all callers destructuring correctly | Compiles |
| T15 | `execute_block_bal` returns None traces | 5th element is `None` | Compiles |

### Integration Verification — Remote compilation (payload_validator.rs)

Due to the complexity of constructing a full EVM + executor + State environment in unit tests,
AC-5 (isolation) and AC-6 (reverted transaction) are verified via:
1. **Code inspection**: `fuse()` is called after each tx's trace extraction — guaranteed by
   the implementation pattern (extract → push → fuse loop).
2. **Type-system guarantee**: `Inspector = TracingInspector` bound ensures the inspector is
   correctly wired; `geth_call_traces` produces a complete `CallFrame` including error info
   for reverted txs (this is `revm-inspectors` library behavior, not our logic).
3. **Remote compilation + existing test suite**: compile and run on remote machine to verify
   no regression in existing engine-tree tests.

| # | Verification | AC covered | Method |
|---|-------------|-----------|--------|
| V1 | Isolation between txs | AC-5 | `fuse()` after each tx trace extraction resets inspector state. Verified by code pattern and compilation. |
| V2 | Reverted tx has trace | AC-6 | `TracingInspector` records all calls regardless of revert. `CallFrame` includes `error` field populated by `revm-inspectors`. Verified by library contract and type. |
| V3 | Empty block produces `Some(vec![])` | AC-8 | When tx iterator is empty, `tx_traces` vec stays empty, returned as `Some(vec![])`. Verified by unit test T12 at chain-state level + code inspection at engine-tree level. |

### CI Verification

| # | Check | Command |
|---|-------|---------|
| C1 | Formatting | `cargo +nightly fmt --all --check` |
| C2 | Feature-gated compilation | `cargo check -p reth-execution-types && cargo check -p reth-chain-state` (without traces) |
| C3 | Full compilation | `cargo check -p reth-engine-tree --all-features` |
| C4 | Unit tests | `cargo nextest run -p reth-execution-types -p reth-chain-state` |

## ADR-1: New `execute_transactions_traced` instead of modifying `execute_transactions`

- **Status**: Adopted (carried from v2.3.0/v2.4.0)
- **Background**: The transaction execution loop needs per-tx inspector access with specific type
  bounds (Inspector = TracingInspector).
- **Decision**: Create parallel function rather than modifying existing or inlining.
- **Rationale**: Zero risk to existing path; clean type separation; satisfies NFR-3.
- **Consequences**: ~50 lines structural overlap; manual sync if upstream changes the function.

## ADR-2: Feature-gate via `#[cfg(feature = "traces")]`

- **Status**: Adopted (carried from v2.3.0/v2.4.0)
- **Background**: `revm-inspectors` and `alloy-rpc-types-trace` are non-trivial deps. Nodes
  not using ExEx call traces should not pay compile-time or runtime cost.
- **Decision**: Gate all trace-related code and dependencies behind a `traces` Cargo feature.
- **Rationale**: Satisfies NFR-2 (zero overhead when disabled); standard Rust pattern.
- **Consequences**: Feature must be enabled in `reth-engine-tree`'s dep on `reth-chain-state`.
