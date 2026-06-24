# ExEx Internal Transactions — Design Document (v2.3.0 Port)

## Background

Requirements document: `docs/spec/exex-internal-txs.md`

This design ports the ExEx call traces feature from `dev-exex-internal-txs` (v2.2.0) to
`dev-exex-internal-txs-v2.3.0` (v2.3.0). The core concept is unchanged: inject a
`TracingInspector` during Engine path block execution, collect per-transaction call frames, and
propagate them through `ExecutedBlock` → `Chain` → `ExExNotification`.

The v2.3.0 codebase introduces structural changes that require adaptation:
1. Transaction execution loop extracted into `execute_transactions()` function
2. New BAL parallel execution path (`execute_block_bal`)
3. Minor API changes (state hook installation, imports)

## Goal

- Satisfy FR-1 through FR-4 and NFR-1, NFR-2
- Minimal change surface to v2.3.0 code
- Pipeline path, `ExExNotification` struct, and `Chain::new` signature remain unchanged
- Existing `execute_transactions` function remains unmodified and is still called when `traces`
  feature is disabled (cfg conditional compilation at the call site)

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
│  spawn_deferred_trie_task()                                 │
│    └── ExecutedBlock.call_traces = call_traces              │
├─────────────────────────────────────────────────────────────┤
│ crates/chain-state/src/in_memory.rs                         │
│                                                             │
│  ExecutedBlock { ..., call_traces: Option<Vec<CallFrame>> } │
│  blocks_to_chain(blocks, include_traces) → Chain            │
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
    blocks: BTreeMap<BlockNumber, RecoveredBlock<N::Block>>,
    execution_outcome: ExecutionOutcome<N::Receipt>,
    trie_data: BTreeMap<BlockNumber, LazyTrieData>,
    /// Call traces per block, collected during Engine path execution.
    /// `None` for historical (pipeline) blocks, BAL-executed blocks, and reorg old-chain.
    /// Not persisted to WAL (skipped by serde).
    #[cfg(feature = "traces")]
    #[cfg_attr(feature = "serde", serde(skip))]
    call_traces: Option<BTreeMap<BlockNumber, Vec<CallFrame>>>,
}

impl<N: NodePrimitives> Chain<N> {
    /// Attaches call traces collected during Engine path execution.
    #[cfg(feature = "traces")]
    pub fn with_call_traces(mut self, traces: BTreeMap<BlockNumber, Vec<CallFrame>>) -> Self;

    /// Returns call traces for all blocks, if available.
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
    pub trie_data: DeferredTrieData,
    /// Call traces collected during Engine path execution.
    /// Always populated for sequential Engine path blocks.
    /// `None` for pipeline blocks and BAL-executed blocks.
    #[cfg(feature = "traces")]
    pub call_traces: Option<Vec<CallFrame>>,
}
```

### Modified: `blocks_to_chain` signature

```rust
// crates/chain-state/src/in_memory.rs

fn blocks_to_chain(blocks: &[ExecutedBlock<N>], include_traces: bool) -> Chain<N>;
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
    E: BlockExecutor<Receipt = N::Receipt, Evm: alloy_evm::Evm<DB = &'a mut State<DB>>>,
    // Inspector must be TracingInspector for geth_call_traces access
    <E::Evm as alloy_evm::Evm>::Inspector: /* TracingInspector methods */,
    Tx: alloy_evm::block::ExecutableTx<E> + alloy_evm::RecoveredTx<InnerTx>,
    InnerTx: TxHashRef,
    DB: revm::Database + 'a,
    Err: core::error::Error + Send + Sync + 'static;
```

### Modified: `execute_block` return type

```rust
// Before (v2.3.0):
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
    Option<Vec<CallFrame>>,  // NEW: call traces
), InsertBlockErrorKind>
```

## Data Structures

### CallFrame (from `alloy-rpc-types-trace`)

External type, not modified. This is the **same type** used by the RPC layer for
`debug_traceTransaction?tracer=callTracer`, which guarantees FR-3 format compatibility at the
type-system level. Key fields:
- `typ`: call type (CALL, STATICCALL, DELEGATECALL, CREATE, etc.)
- `from`, `to`: addresses
- `value`: transfer value
- `gas`, `gas_used`: gas accounting
- `input`, `output`: calldata bytes
- `calls`: nested call tree (recursive)

**Trace content control**: `TracingInspectorConfig::none()` disables opcode/memory/stack recording
but still records the call tree structure (from/to/value/gas/type/calls). The `input` and `output`
fields in the resulting `CallFrame` are controlled by `CallConfig::default()` passed to
`geth_call_traces()` — with default options, `input` and `output` are **included** (matching the
behavior of Geth's callTracer with no special options). This keeps the trace format faithful to FR-3
while `TracingInspectorConfig::none()` ensures NFR-1 by avoiding the expensive opcode-level
recording.

### Data ownership

| Data | Owner (write) | Readers |
|------|---------------|---------|
| `ExecutedBlock.call_traces` | `payload_validator.rs` (after execution) | `in_memory.rs` (blocks_to_chain) |
| `Chain.call_traces` | `in_memory.rs` (blocks_to_chain) | ExEx subscribers (via notification) |

### Data format and boundaries

| Field | Type | Nullable | Default | Range |
|-------|------|----------|---------|-------|
| `ExecutedBlock.call_traces` | `Option<Vec<CallFrame>>` | Yes (Option) | `None` | 0..N frames (one per tx) |
| `Chain.call_traces` | `Option<BTreeMap<BlockNumber, Vec<CallFrame>>>` | Yes (Option) | `None` | Keyed by block numbers in chain |

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
2. Add `call_traces` field to `Chain` struct (cfg-gated, `#[cfg_attr(feature = "serde", serde(skip))]`)
3. Add `with_call_traces()` and `call_traces()` methods
4. Update `Default` impl and all constructors to initialize `call_traces: None`
5. Update `Chain::new()` to include the field
6. In `append_chain`: add `#[cfg(feature = "traces")] debug_assert!(other.call_traces.is_none())`
   to guard against silently dropping traces from the appended chain
7. Update `serde_bincode_compat` module: add `#[cfg(feature = "traces")] #[serde(skip)] call_traces: ()`
   to the bincode-compat `Chain` representation so roundtrip is stable. Also update
   `From<Chain<'a, N>> for super::Chain<N>` impl to include
   `#[cfg(feature = "traces")] call_traces: None` in the struct literal construction
8. `PartialEq`: the derived `PartialEq` on `Chain` INCLUDES `call_traces` in comparison (when
   `traces` feature is active). This is intentional — two chains with different traces are
   semantically different for ExEx consumers

### Step 2: Data model — `reth-chain-state` (in_memory.rs)

1. Add `traces` feature to Cargo.toml, depending on `reth-execution-types/traces`
2. Add `call_traces` field to `ExecutedBlock` (cfg-gated, `pub` visibility)
3. Update `ExecutedBlock::new()` and `with_deferred_trie_data()` to initialize `None`
4. `PartialEq` for `ExecutedBlock`: the manual impl EXCLUDES `call_traces` from comparison
   (traces are observability data, not block identity)
5. Modify `blocks_to_chain` signature to accept `include_traces: bool`; add
   `#[cfg(not(feature = "traces"))] let _ = include_traces;` to suppress unused-variable warning
6. Inside `blocks_to_chain`, collect traces from blocks and call `chain.with_call_traces()`
7. Update `to_chain_notification`: pass `true` for new chains, `false` for old chains in reorgs

### Step 3: Execution — `reth-engine-tree` (payload_validator.rs)

1. Add `alloy-rpc-types-trace` and `revm-inspectors` to Cargo.toml
2. Enable `reth-chain-state/traces` feature
3. In `execute_block`:
   - Replace `evm_with_env` with `evm_with_env_and_inspector(db, env, TracingInspector::new(TracingInspectorConfig::none()))`
   - Use `#[cfg(feature = "traces")]` conditional compilation at the call site:
     - When `traces` is enabled: call `execute_transactions_traced`, destructure 3-tuple
     - When `traces` is disabled: call `execute_transactions` (original path, zero overhead)
     ```rust
     #[cfg(feature = "traces")]
     let (executor, senders, call_traces) = self.execute_transactions_traced(...)?;
     #[cfg(not(feature = "traces"))]
     let (executor, senders) = self.execute_transactions(...)?;
     #[cfg(not(feature = "traces"))]
     let call_traces: Option<Vec<CallFrame>> = None;
     ```
   - Add `Option<Vec<CallFrame>>` to return tuple
4. Implement `execute_transactions_traced`:
   - Same logic as `execute_transactions` — **implementation note: copy from v2.3.0's
     `execute_transactions` (NOT the v2.2.0 inlined loop) as the base, then add tracing logic.
     This ensures BAL `bump_bal_index` and any other v2.3.0 additions are preserved.**
   - After `apply_pre_execution_changes`: call `executor.evm_mut().inspector_mut().fuse()`
   - After each `execute_transaction`: extract gas_used from GasOutput, call
     `geth_builder().geth_call_traces(Default::default(), gas_used)`, push to vec, call `.fuse()`
   - Return `(executor, senders, tx_traces)`
5. In `execute_block_bal`: return `None` for call_traces
6. At the call site (validation flow): destructure 5-tuple, pass `call_traces` to block
   construction
7. After `spawn_deferred_trie_task`: assign `executed_block.call_traces = call_traces`

### Step 4: Documentation

1. Copy `docs/design/exex-internal-txs.md` (this file)
2. Ensure `docs/spec/exex-internal-txs.md` is present (already done)

## Alternative Approaches Considered

### Alternative 1: Modify `execute_transactions` with a callback parameter

Add `on_tx_executed: impl FnMut(&mut E, GasOutput)` to `execute_transactions`.

**Rejected because:**
- Requires additional type bounds on the executor's Inspector associated type
- `FnMut` closure capturing `Vec<CallFrame>` creates borrow-checker friction with the executor's
  `&mut self` reference
- Adds complexity to a function signature that's already heavily generic
- Risk of breaking the non-tracing path if bounds are wrong

### Alternative 2: Inline the transaction loop in `execute_block`

Remove the call to `execute_transactions` and write the loop directly in `execute_block`.

**Rejected because:**
- Reverses the v2.3.0 refactoring that extracted `execute_transactions`
- Makes `execute_block` significantly longer
- Future changes to `execute_transactions` would need to be duplicated

### Chosen: New `execute_transactions_traced` function

**Rationale:**
- Zero risk to the existing non-tracing path
- Clean type bounds (can require TracingInspector explicitly)
- ~50 lines of code that differ meaningfully from `execute_transactions` (not pure duplication)
- Matches the project pattern of separate functions for different execution modes
  (cf. `execute_block` vs `execute_block_bal`)

## Concurrency Analysis

- **TracingInspector**: owned solely by the EVM instance within `execute_block`; no shared access
- **`ExecutedBlock.call_traces`**: written once after execution, then only read through `Arc`
- **`Chain.call_traces`**: built in `blocks_to_chain` (single-threaded), then shared via
  `Arc<Chain>` (read-only)
- **BAL parallel workers**: each worker has its own EVM instance; no tracing inspector injected,
  so no concurrent inspector access concern

No new concurrent mutable state is introduced.

## Error Handling

- If `executor.finish()` fails after traces are collected, traces are discarded (logged at debug
  level). This matches the v2.2.0 behavior.
- `TracingInspector` cannot fail — it passively records calls. No new error paths are introduced
  in the execution flow.

## Known Limitations and Risks

1. **~50 lines of structural overlap** between `execute_transactions` and
   `execute_transactions_traced`. If upstream modifies `execute_transactions`, the traced version
   must be updated manually. Add a code comment in both functions cross-referencing each other to
   aid future synchronization.
2. **BAL path returns no traces** — consumers must handle `None`. This is documented and
   consistent with pipeline blocks.
3. **Performance**: `TracingInspectorConfig::none()` disables opcode/memory/stack recording but
   still records call tree structure. The per-opcode inspector dispatch (`step()`/`step_end()`)
   remains the primary overhead source. Overhead is expected to be < 15% on average mainnet
   blocks; call-heavy blocks (MEV, aggregators) may reach 20%. Empirical benchmark to be added
   post-merge as follow-up.
4. **Memory**: Large blocks with deep call trees produce large `Vec<CallFrame>`. No cap is
   enforced; this is acceptable because Ethereum gas limits bound the practical depth. On typical
   mainnet blocks, trace data is 1-5MB per block. On pathological blocks (heavy MEV, aggregator
   contracts with 50+ internal calls per tx), traces may reach 50-100MB per block. With 128
   blocks in the in-memory state, worst-case steady-state memory for traces is several GB. A
   per-block trace size cap is deferred to a future iteration (matches v2.2.0 behavior which
   also has no cap).
5. **Feature gate (`#[cfg(feature = "traces")]`)**: The `call_traces` field and related code are
   gated behind a `traces` Cargo feature. Rationale: avoids pulling `alloy-rpc-types-trace` and
   `revm-inspectors` as transitive dependencies for nodes that do not use ExEx or do not need
   call traces. ExEx consumers that want traces must enable the `traces` feature on
   `reth-chain-state`. This follows the v2.2.0 implementation and is consistent with reth's
   pattern of optional features for heavyweight dependencies.
6. **WAL recovery loses traces**: After node restart, blocks recovered from WAL have
   `call_traces = None` (`#[serde(skip)]`). This is indistinguishable from pipeline/BAL blocks.
   ExEx consumers that require trace completeness should track block numbers and use RPC backfill
   (`debug_traceBlock`) as recovery strategy. Matches v2.2.0 behavior.

## Test Plan

### Unit Tests — `reth-execution-types` (chain.rs)

| # | Case | Input | Expected |
|---|------|-------|----------|
| T1 | `Chain` default has no traces | `Chain::default()` | `call_traces()` returns `None` |
| T2 | `with_call_traces` attaches traces | Chain + BTreeMap with 2 blocks | `call_traces()` returns `Some` with both blocks |
| T3 | `Chain::new` initializes traces to None | `Chain::new(blocks, outcome, trie)` | `call_traces()` returns `None` |
| T3a | `Chain::new` then `with_call_traces` lifecycle | `Chain::new(...)` followed by `.with_call_traces(traces)` | `call_traces()` returns `Some`, field values match input |
| T3b | `call_traces` skipped in bincode roundtrip | Chain with traces, serialize/deserialize via bincode compat | Deserialized chain has `call_traces() == None` (serde skip) |

### Unit Tests — `reth-chain-state` (in_memory.rs)

| # | Case | Input | Expected |
|---|------|-------|----------|
| T4 | `blocks_to_chain` injects traces | 2 ExecutedBlocks with `call_traces = Some(...)`, `include_traces = true` | Chain has traces keyed by correct block numbers |
| T5 | `blocks_to_chain` no traces when all None | 2 ExecutedBlocks with `call_traces = None`, `include_traces = true` | Chain `call_traces()` is `None` |
| T6 | `blocks_to_chain` partial traces | Mix of Some/None blocks, `include_traces = true` | Chain has traces only for blocks with Some |
| T7 | `blocks_to_chain` suppresses traces | Blocks with Some traces, `include_traces = false` | Chain `call_traces()` is `None` |
| T7a | `blocks_to_chain` traces cleared then None | ExecutedBlock with traces set then overwritten to None, `include_traces = true` | Chain `call_traces()` is `None` |
| T7b | `append_chain` preserves self traces | Chain A with traces, append Chain B (no traces) | Chain A still has original traces |
| T8 | `to_chain_notification` Commit has traces | NewCanonicalChain::Commit with traced blocks | `new.call_traces()` is `Some` |
| T9 | `to_chain_notification` Reorg old has no traces | NewCanonicalChain::Reorg | `old.call_traces()` is `None`, `new.call_traces()` is `Some` |
| T10a | `to_chain_notification` Revert old has no traces | NewCanonicalChain::Reorg (reverted blocks) | Reverted chain's `call_traces()` is `None` |

### Unit Tests — `reth-engine-tree` (payload_validator.rs)

| # | Case | Input | Expected |
|---|------|-------|----------|
| T10 | `execute_transactions_traced` collects per-tx frames | Executor with TracingInspector, 3 transactions | Returns `Vec<CallFrame>` with 3 entries |
| T11 | `execute_transactions_traced` fuses between transactions | 2 transactions with nested calls | Each CallFrame contains only its own call tree, no bleed from previous tx |
| T12 | `execute_transactions_traced` fuses after pre-execution | Block with beacon root update | Pre-execution system calls do not appear in first tx's trace |
| T13 | `execute_block` returns call_traces in 5-tuple | Normal block execution | 5th element is `Some(Vec<CallFrame>)` |
| T14 | `execute_block_bal` returns None for traces | BAL-eligible block | call_traces position in return is `None` |

### Integration Tests — `reth-engine-tree`

These tests require a real EVM executor (not MockEvmConfig) with in-memory state and deployed
contracts. They validate correctness of inspector wiring, not just data flow.

| # | Case | Input | Expected |
|---|------|-------|----------|
| T15 | End-to-end: ExEx receives traces via notification | Execute block via engine, observe ExExNotification | `chain.call_traces()` is `Some`, contains correct number of frames |
| T16 | Multi-level nested call content correctness | Contract A CALL B, B DELEGATECALL C | Root CallFrame: from=sender, to=A, typ="CALL"; calls[0]: from=A, to=B, typ="CALL"; calls[0].calls[0]: from=B, to=C, typ="DELEGATECALL" |
| T17 | Reverted transaction produces trace with error | Transaction that reverts | CallFrame present with `error` field non-None |
| T18 | gas_used field correctness | Simple known-gas transaction | `CallFrame.gas_used > 0` and consistent with receipt gas_used |
| T19 | Empty block (0 user transactions) | Block with only pre-execution system calls | `call_traces = Some(vec![])` |
| T20 | All-revert block | Block where every tx reverts | `call_traces = Some(vec![...])`, each frame has error field |

Note: If integration test infrastructure proves too complex to set up for initial merge,
T16-T20 may be deferred to a follow-up PR, but this must be explicitly documented in the
merge PR description.

### CI Checks (standard verification)

| # | Check | Command |
|---|-------|---------|
| C1 | Code formatting | `cargo +nightly fmt --all --check` |
| C2 | Lints | `cargo +nightly clippy --workspace --all-features` |
| C3 | Test suite | `cargo nextest run -p reth-chain-state -p reth-execution-types -p reth-engine-tree` |

## ADR-1: New function `execute_transactions_traced` instead of modifying existing `execute_transactions`

- **Status**: Adopted
- **Background**: v2.3.0 extracted the transaction loop into `execute_transactions()`. Our tracing
  feature needs per-tx inspector access after each transaction, which requires different type bounds
  (Inspector = TracingInspector) and additional logic.
- **Decision**: Create a parallel function `execute_transactions_traced` rather than modifying the
  existing function or inlining the loop.
- **Rationale**: Zero risk to existing execution path; clean type separation; matches the project's
  pattern of separate functions for different execution modes. The ~50 lines of structural overlap
  is acceptable given that the functions have meaningfully different behavior.
- **Consequences**: If upstream modifies `execute_transactions`, the traced variant must be updated
  manually. This is a low-frequency maintenance cost since the function is stable.
