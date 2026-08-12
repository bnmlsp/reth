# ExEx Internal Transactions — Design Document (v2.4.0 Port)

## Background

Requirements document: `docs/spec/exex-internal-txs.md`

This design ports the ExEx call traces feature from `dev-exex-internal-txs-v2.3.0` (v2.3.0) to
`dev-exex-internal-txs-v2.4.0` (v2.4.0). The core concept is unchanged: inject a
`TracingInspector` during Engine path block execution, collect per-transaction call frames, and
propagate them through `ExecutedBlock` → `Chain` → `ExExNotification`.

The v2.4.0 codebase introduces structural changes that require adaptation:
1. `payload_validator.rs` heavily restructured — new validation pipeline with `StateRootStrategy`,
   spawned background tasks (`payload-convert`, `tx-iterator`, `prewarm`, etc.)
2. `DeferredTrieData` renamed to `LazyTrieData`
3. `Chain.blocks` type changed from `RecoveredBlock` to `Arc<RecoveredBlock>`
4. `blocks_to_chain()` simplified — removed closure wrapping for trie data

## Goal

- Satisfy FR-1 through FR-4 and NFR-1 (Should), NFR-2 (Must)
- Minimal change surface to v2.4.0 code
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
│  blocks_to_chain(blocks) → Chain  (simplified, no bool)     │
│  to_chain_notification() → CanonStateNotification           │
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

### Modified: `blocks_to_chain` (v2.4.0 simplification)

```rust
// crates/chain-state/src/in_memory.rs
// v2.4.0: NO `include_traces` parameter needed.
// Reorg old-chain blocks have call_traces = None, so filter_map naturally skips them.

fn blocks_to_chain(blocks: &[ExecutedBlock<N>]) -> Chain<N> {
    // ... existing logic ...
    #[cfg(feature = "traces")]
    {
        let traces: BTreeMap<_, _> = blocks
            .iter()
            .filter_map(|b| b.call_traces.as_ref().map(|t| (b.block_number(), t.clone())))
            .collect();
        if !traces.is_empty() {
            chain = chain.with_call_traces(traces);
        }
    }
    chain
}
```

### New: `execute_transactions_traced` function

```rust
// crates/engine/tree/src/tree/payload_validator.rs

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

### Modified: `execute_block` return type

```rust
// v2.4.0 original:
fn execute_block(...) -> Result<(
    BlockExecutionOutput<N::Receipt>,
    Vec<Address>,
    ReceiptRootReceiver,
    Option<BlockAccessList>,
), InsertBlockErrorKind>

// After modification:
fn execute_block(...) -> Result<(
    BlockExecutionOutput<N::Receipt>,
    Vec<Address>,
    ReceiptRootReceiver,
    Option<BlockAccessList>,
    Option<Vec<CallFrame>>,  // NEW
), InsertBlockErrorKind>
```

## Data Structures

### CallFrame (from `alloy-rpc-types-trace`)

External type, not modified. Same type used by RPC `debug_traceTransaction?tracer=callTracer`,
guaranteeing FR-3 format compatibility at the type-system level. Key fields:
- `typ`: call type (CALL, STATICCALL, DELEGATECALL, CREATE, etc.)
- `from`, `to`: addresses
- `value`: transfer value
- `gas`, `gas_used`: gas accounting
- `input`, `output`: calldata bytes
- `calls`: nested call tree (recursive)

**Trace content control**: `TracingInspectorConfig::none()` disables opcode/memory/stack recording
but still records the call tree structure. `CallConfig::default()` passed to `geth_call_traces()`
includes `input` and `output` fields, matching Geth's callTracer default behavior.

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
  ├── reth-chain-state (features: ["rayon", "traces"])  ← add "traces"
  ├── alloy-rpc-types-trace (new dep)
  └── revm-inspectors (new dep)

reth-chain-state
  └── reth-execution-types (features: ["traces"])       ← propagate

reth-execution-types
  └── alloy-rpc-types-trace (new dep, optional under "traces" feature)
```

## Detailed Implementation Plan

### Step 1: Data model — `reth-execution-types` (chain.rs)

1. Add `alloy-rpc-types-trace` as optional dependency under `traces` feature
2. Add `call_traces` field to `Chain` struct (cfg-gated, serde skip)
3. Add `with_call_traces()` and `call_traces()` methods
4. Update `Default` impl and `new()` to initialize `call_traces: None`
5. In `append_chain`: add `debug_assert!(other.call_traces.is_none())`
6. Update `serde_bincode_compat` module: add skip field and update From impls

### Step 2: Data model — `reth-chain-state` (in_memory.rs)

1. Add `traces` feature to Cargo.toml (propagates to `reth-execution-types/traces`)
2. Add `call_traces` field to `ExecutedBlock` (cfg-gated, `pub`)
3. Update constructors (`Default`, `new()`, `with_deferred_trie_data()`) — all init `None`
4. `with_deferred_trie_data` remains `const fn` (`None` is a valid const value)
5. In `blocks_to_chain`: append trace collection at the end (cfg-gated block)
6. `to_chain_notification()`: no changes needed — blocks in old chain naturally have
   `call_traces = None`, so `filter_map` in `blocks_to_chain` produces empty map → no traces

### Step 3: Execution — `reth-engine-tree` (payload_validator.rs)

1. Add `alloy-rpc-types-trace` and `revm-inspectors` to Cargo.toml
2. Enable `reth-chain-state/traces` feature
3. In `execute_block`:
   - Replace `evm_with_env` with `evm_with_env_and_inspector(..., TracingInspector::new(...))`
   - Call `execute_transactions_traced` instead of `execute_transactions`
   - Add `Option<Vec<CallFrame>>` to return tuple
4. Implement `execute_transactions_traced`:
   - Based on v2.4.0's `execute_transactions` function
   - After `apply_pre_execution_changes`: `fuse()` to discard system call traces
   - After each `execute_transaction`: extract `gas_used`, call
     `geth_builder().geth_call_traces(Default::default(), gas_used)`, push, `fuse()`
   - Return `(executor, senders, tx_traces)`
5. In `execute_block_bal`: add `None` for call_traces in return tuple
6. In `validate_block_with_state`: destructure 5-tuple, assign `executed_block.call_traces`
   after `spawn_deferred_trie_task`
7. Mark original `execute_transactions` as `#[allow(dead_code)]` (kept for upstream merge)

### Step 4: Documentation

- `docs/spec/exex-internal-txs.md` — already in place
- `docs/design/exex-internal-txs.md` — this file

## Alternative Approaches

### Alternative: Modify `execute_transactions` with callback/inspector parameter

**Rejected**: Adds type complexity to an already heavily generic function; risks breaking
non-tracing path; borrow-checker friction with `&mut executor` + closure capturing trace vec.

### Alternative: Inline transaction loop in `execute_block`

**Rejected**: Reverses v2.3.0+ refactoring; makes `execute_block` too long; duplicates future
upstream changes.

### Chosen: New `execute_transactions_traced` function

Same rationale as v2.3.0: zero risk to existing path, clean type bounds, matches project pattern.

## v2.4.0 Specific Adaptation Notes

Compared to v2.3.0 design, the following adaptations are needed:

| Item | v2.3.0 | v2.4.0 |
|------|--------|--------|
| `blocks_to_chain` signature | `(blocks, include_traces: bool)` | `(blocks)` — no bool needed |
| `to_chain_notification` | Pass `true`/`false` for include_traces | No changes needed |
| Reorg old chain traces | Explicitly suppressed (`include_traces: false`) | Included if present — intentional simplification (see note below) |
| `ExecutedBlock.trie_data` type | `DeferredTrieData` | `LazyTrieData` |
| `Chain.blocks` value type | `RecoveredBlock<N::Block>` | `Arc<RecoveredBlock<N::Block>>` |
| `with_deferred_trie_data` | was `const fn`, became non-const in v2.3.0 | `const fn` in v2.4.0, stays const |
| `execute_block` state_hook | Internal to handle | Explicit parameter |
| `spawn_deferred_trie_task` 5th param | `changeset_provider` | `changed_paths: Option<Arc<TriePrefixSetsMut>>` |
| trace assignment point | Inside `spawn_deferred_trie_task` | After `spawn_deferred_trie_task` return |

## Concurrency Analysis

- **TracingInspector**: owned solely by the EVM instance within `execute_block`; no shared access
- **`ExecutedBlock.call_traces`**: written once after execution, then only read through `Arc`
- **`Chain.call_traces`**: built in `blocks_to_chain` (single-threaded), then shared via
  `Arc<Chain>` (read-only)
- **BAL parallel workers**: each has its own EVM; no tracing inspector injected

No new concurrent mutable state is introduced.

## Error Handling

- `TracingInspector` cannot fail — it passively records calls. No new error paths introduced.
- If `executor.finish()` fails after traces are collected, traces are discarded along with the
  entire execution result (existing behavior, no special handling needed).

## Known Limitations and Risks

1. **~50 lines of structural overlap** between `execute_transactions` and
   `execute_transactions_traced`. Cross-reference comments in both functions.
2. **BAL path returns no traces** — documented, consistent with pipeline blocks.
3. **Performance**: `TracingInspectorConfig::none()` still records call tree. Expected < 15%
   overhead on average blocks. Not benchmarked this iteration (NFR-1 is Should).
4. **Memory**: No trace size cap. Typical mainnet blocks: 1-5MB per block. Pathological: up to
   50-100MB. With 128 in-memory blocks, worst-case several GB. Deferred to future iteration.
5. **Feature gate**: `#[cfg(feature = "traces")]` avoids pulling heavy deps for non-ExEx nodes.
6. **WAL recovery loses traces**: Blocks from WAL have `call_traces = None` (serde skip).
7. **Reorg old chain includes traces** (v2.3.0 behavioral change): In v2.3.0, reorg notifications
   explicitly suppressed traces for the old (reverted) chain via `blocks_to_chain(blocks, false)`.
   In v2.4.0, the `include_traces` parameter is removed; old chain blocks that were previously
   executed via the engine path will retain their traces in the reorg notification. ExEx consumers
   can distinguish `old` vs `new` via the `ChainReorged { old, new }` variant and choose whether
   to use old chain traces. This is an intentional simplification — more information is available
   to consumers without extra API surface.

## Test Plan

### Unit Tests — `reth-execution-types` (chain.rs)

| # | Case | Input | Expected |
|---|------|-------|----------|
| T1 | Chain default has no traces | `Chain::default()` | `call_traces()` returns `None` |
| T2 | `with_call_traces` attaches traces | Chain + BTreeMap with 2 blocks | `call_traces()` returns `Some` with both |
| T3 | `Chain::new` initializes traces to None | `Chain::new(blocks, outcome, trie)` | `call_traces()` returns `None` |
| T4 | Replace semantics | Chain with traces, call `with_call_traces` again | Old traces replaced |
| T5 | Bincode roundtrip skips traces | Serialize/deserialize | `call_traces()` is `None` after roundtrip |

### Unit Tests — `reth-chain-state` (in_memory.rs)

| # | Case (function name) | Input | Expected |
|---|------|-------|----------|
| T6 | `test_blocks_to_chain_injects_traces` | Blocks with `call_traces = Some(...)` | Chain has matching traces |
| T7 | `test_blocks_to_chain_no_traces_when_none` | Blocks with `call_traces = None` | Chain `call_traces()` is `None` |
| T8 | `test_blocks_to_chain_partial_traces_for_mixed_blocks` | Mix of Some/None blocks | Chain has traces only for Some blocks |
| T9 | `test_to_chain_notification_commit_includes_traces` | Commit with traced blocks | `new.call_traces()` is `Some` |
| T10 | `test_to_chain_notification_reorg_traces` | Reorg with new(traced) + old(no traces) | `new` has traces, `old` is `None` |
| T10b | `test_to_chain_notification_revert_no_traces` | Revert with old blocks | old chain `call_traces()` is `None` |
| T11 | `test_set_call_traces_field` | Set traces then clear to None | Behaves correctly |

### Unit Tests — `reth-engine-tree` (payload_validator.rs)

| # | Case | Input | Expected |
|---|------|-------|----------|
| T12 | `execute_transactions_traced` collects frames | 3 transactions | Vec with 3 CallFrames |
| T13 | Fuse between transactions | 2 txs with nested calls | No bleed between frames |
| T14 | Fuse after pre-execution | Block with beacon root update | System calls not in first tx trace |
| T15 | `execute_block` returns 5-tuple | Normal block | 5th element is `Some(Vec<CallFrame>)` |
| T16 | `execute_block_bal` returns None | BAL block | call_traces is `None` |
| T17 | Reverted tx still has trace | Tx that reverts | CallFrame present with error info |
| T18 | Empty block (0 user txs) | Block with no txs | `call_traces = Some(vec![])` |

### CI Checks

| # | Check | Command |
|---|-------|---------|
| C1 | Formatting | `cargo +nightly fmt --all --check` |
| C2 | Lints | `cargo +nightly clippy --workspace --all-features` |
| C3 | Tests | `cargo nextest run -p reth-chain-state -p reth-execution-types -p reth-engine-tree` |

## ADR-1: New `execute_transactions_traced` instead of modifying `execute_transactions`

- **Status**: Adopted (carried from v2.3.0)
- **Background**: The transaction execution loop needs per-tx inspector access with specific type
  bounds (Inspector = TracingInspector).
- **Decision**: Create parallel function rather than modifying existing or inlining.
- **Rationale**: Zero risk to existing path; clean type separation; matches project pattern.
- **Consequences**: ~50 lines structural overlap; manual sync if upstream changes the function.

## ADR-2: Remove `include_traces` parameter from `blocks_to_chain` (v2.4.0 change)

- **Status**: Adopted
- **Background**: In v2.3.0, `blocks_to_chain(blocks, include_traces: bool)` was needed to
  suppress traces for reorg old-chain. In v2.4.0, reorg old-chain blocks naturally have
  `call_traces = None` (they were loaded from persistence, not freshly executed).
- **Decision**: Remove the `bool` parameter. Use `filter_map` to skip blocks with `None` traces.
- **Rationale**: Simpler API; no caller needs to decide; behavior is determined by data, not flags.
- **Consequences**: If future code constructs old-chain `ExecutedBlock` with non-None traces
  (unexpected), they would be included. Mitigated by the fact that only `payload_validator.rs`
  sets `call_traces`.
