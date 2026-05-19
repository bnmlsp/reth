# ExEx Internal Transactions — Design Document

## Background

Requirements document: `docs/spec/exex-internal-txs.md`

ExEx currently receives block/receipts/state diff via `ExExNotification::ChainCommitted`, but does not include call traces (internal transaction trees). This feature injects a `TracingInspector` during Engine path (newPayload) block execution, attaches call traces to the `Chain` struct, and pushes them to ExEx subscribers.

## Goals

- Satisfy FR-1 through FR-4 and NFR-1, NFR-2
- Minimal change surface: ~70–90 lines of new code
- Pipeline path, `ExExNotification` struct, and `Chain::new` signature remain unchanged
- NFR-1 (≤ 15% CPU overhead): enforced via minimal `TracingInspectorConfig` (opcode/memory/stack recording disabled); the exact figure will be validated post-launch via benchmarking and is out of scope for this test plan

## Interface Definitions

### New: `Chain::call_traces` field

```rust
// crates/evm/execution-types/src/chain.rs
pub struct Chain<N: NodePrimitives = EthPrimitives> {
    blocks: BTreeMap<BlockNumber, RecoveredBlock<N::Block>>,
    execution_outcome: ExecutionOutcome<N::Receipt>,
    trie_data: BTreeMap<BlockNumber, LazyTrieData>,
    /// Call traces per block, collected during Engine path execution.
    /// None for historical (pipeline) blocks and reorg old-chain.
    #[cfg_attr(feature = "serde", serde(skip))]
    call_traces: Option<BTreeMap<BlockNumber, Vec<CallFrame>>>,
}
```

### New: `Chain::with_call_traces` setter

```rust
impl<N: NodePrimitives> Chain<N> {
    /// Attaches call traces collected during Engine path execution.
    pub fn with_call_traces(
        mut self,
        traces: BTreeMap<BlockNumber, Vec<CallFrame>>,
    ) -> Self {
        self.call_traces = Some(traces);
        self
    }

    /// Returns call traces for all blocks, if available.
    pub fn call_traces(&self) -> Option<&BTreeMap<BlockNumber, Vec<CallFrame>>> {
        self.call_traces.as_ref()
    }
}
```

## Data Structures

### `CallFrame`

From `alloy_rpc_types::eth::geth::call::CallFrame` (`alloy-rpc-types` v2.0.4, already in workspace). Fields:
`type`, `from`, `to`, `value`, `gas`, `gas_used`, `input`, `output`, `error`, `revert_reason`, `calls: Vec<CallFrame>`.

Directly matches the output of `debug_traceTransaction?tracer=callTracer` (FR-3).

Note: the field name `call_traces` follows the "internal transactions / call traces" terminology used in the requirements document; the storage type `CallFrame` is the standard Geth callTracer output type. There is no naming conflict.

**NFR-2 (backward compatibility) verification:** `call_traces: Option<...>` defaults to `None`; the 66 call sites of `Chain::new()` have unchanged signatures; existing `ExecutedBlock::new()` call sites require no modification. Verified via CI full workspace compilation (`cargo check --workspace --all-features`).

Add to `crates/evm/execution-types/Cargo.toml`:
```toml
alloy-rpc-types = { workspace = true, optional = true, features = ["eth"] }
```
Included under the `default` / `std` features (together with `revm-inspectors` under a `traces` feature gate).

### `TracingInspectorConfig` (minimal)

```rust
TracingInspectorConfig::none()
    .with_record_calls(true)
    .with_record_logs(false)  // logs are already in receipts; avoid double collection
```

This configuration disables opcode/memory/stack recording, satisfying NFR-1 (≤ 15% overhead).

## Module Breakdown and Change List

| File | Change | Description |
|------|--------|-------------|
| `crates/evm/execution-types/src/chain.rs` | Modify | Add `call_traces` field, `with_call_traces()` setter, `call_traces()` getter; serde skip annotation |
| `crates/evm/execution-types/src/chain.rs` `serde_bincode_compat` | Modify | Add `#[serde(skip)] call_traces` field to bincode repr (WAL round-trips transparently; traces not persisted to WAL) |
| `crates/evm/execution-types/Cargo.toml` | Modify | Add optional `alloy-rpc-types` dependency (features = ["eth"]); add optional `revm-inspectors` dependency (workspace) |
| `crates/engine/tree/src/tree/payload_validator.rs` | Modify | `execute_block()` always injects `TracingInspector`; single inline transaction loop extracts `CallFrame` + `fuse()` per tx; `execute_transactions()` removed (was only used in the old no-inspector path); return value extended with `Option<Vec<CallFrame>>` |
| `crates/chain-state/src/in_memory.rs` | Modify | Add `call_traces: Option<Vec<CallFrame>>` field to `ExecutedBlock`; `blocks_to_chain()` reads the field and calls `with_call_traces()` |

**Files that do not need to change:**
- `crates/exex/types/src/notification.rs`: `ExExNotification` struct unchanged
- `crates/evm/evm/src/lib.rs`: no trait changes, `ConfigureEvm` unchanged
- 66 call sites of `Chain::new()`: all unchanged (`call_traces` defaults to `None`, set via setter after construction)

## Execution Path Changes

### New path (always active)

```
execute_block()
  → TracingInspector::new(config)
  → evm_with_env_and_inspector(&mut db, env, inspector)
  → create_executor(evm_with_inspector, ctx)   // concrete type: BlockExecutorForEvm<..., TracingInspector>
  → [inline tracing loop]
      executor.apply_pre_execution_changes()
      for tx in transactions:
          let gas_output = executor.execute_transaction(tx)?
          let gas_used = gas_output.tx_gas_used()
          // alloy_evm::Evm trait provides inspector_mut() via components_mut();
          // executor.evm_mut() returns &mut Self::Evm, .inspector_mut() is a provided method
          let call_frame = executor.evm_mut().inspector_mut()
                               .geth_builder()
                               .geth_call_traces(CallConfig::default(), gas_used)
          tx_traces.push(call_frame)
          executor.evm_mut().inspector_mut().fuse()   // reset arena for next tx
  → (evm, result) = executor.finish()
  → merge_transitions()
  → return (BlockExecutionOutput, Vec<Address>, Receiver<...>, Some(tx_traces))
```

**Why `execute_transactions` is not reused:**
`execute_transactions` was a generic function (`E: BlockExecutor`) that did not expose the inspector. While `alloy_evm::Evm` trait does expose `inspector_mut()` (via the provided `components_mut()` method), threading `TracingInspector`-specific calls (`geth_builder()`, `fuse()`) into the generic helper would require adding an `Inspector`-aware type parameter to it. The chosen approach inlines the tracing-aware transaction loop directly in `execute_block`; `execute_transactions` has been removed as it is no longer called.

**Key API verification (revm-inspectors v0.39.0):**
- `geth_call_traces()` hard-codes `nodes[0]` as the root, assuming only one transaction's trace is in the arena — therefore `fuse()` must be called after each transaction to reset the arena before the next
- `TracingInspector::geth_builder()` — borrows the inspector without consuming it
- `TracingInspector::fuse()` — resets the arena while retaining allocated capacity, O(n) but very cheap
- `gas_used` is obtained via `gas_output.tx_gas_used()` — this matches receipt `gasUsed` and Geth callTracer root `gasUsed` semantics; `state_gas_used()` (Amsterdam EIP-8037) tracks state growth fees separately and must not be included here

**Performance note:** `TracingInspector` collects call traces in real time during EVM execution (the dominant cost). `geth_call_traces()` + `fuse()` are pure in-memory operations; the N extra function calls per block are negligible.

### `blocks_to_chain()` changes

```rust
// in_memory.rs
fn blocks_to_chain(blocks: &[ExecutedBlock<N>]) -> Chain<N> {
    // existing logic unchanged; after building the chain:
    let traces: BTreeMap<_, _> = blocks.iter()
        .filter_map(|b| b.call_traces.as_ref().map(|t| (b.block_number(), t.clone())))
        .collect();
    if traces.is_empty() { chain } else { chain.with_call_traces(traces) }
}
```

Traces flow through the `ExecutedBlock` field. `blocks_to_chain` reads `ExecutedBlock.call_traces` from each block, assembles a `BTreeMap<BlockNumber, Vec<CallFrame>>`, and calls `chain.with_call_traces()`. The `to_chain_notification()` caller requires no changes.

### How traces flow from `execute_block` to `blocks_to_chain`

`execute_transactions` is **not modified**. Traces are collected directly in the inline tracing loop inside `execute_block`.

`execute_block` extended return value:
```rust
(BlockExecutionOutput, Vec<Address>, Receiver<...>, Option<Vec<CallFrame>>)
// Engine path → always Some(tx_traces)
// Pipeline path → always None (execute_block not called on pipeline path)
```

The caller (`on_new_payload` etc.) stores the `Option<Vec<CallFrame>>` in the `call_traces` field when constructing `ExecutedBlock`, which then flows into `blocks_to_chain` to be assembled into `Chain`.

**Rationale for `ExecutedBlock` field approach vs. tuple extension:**
- Pros: traces travel with the block; the call chain is clear
- Cons: `ExecutedBlock` is a public type; the change propagates to all construction sites (though Pipeline path is unaffected since the value is `None`)

`ExecutedBlock` changes:
```rust
pub struct ExecutedBlock<N: NodePrimitives = EthPrimitives> {
    pub recovered_block: Arc<RecoveredBlock<N::Block>>,
    pub execution_output: Arc<BlockExecutionOutput<N::Receipt>>,
    pub trie_data: DeferredTrieData,
    /// Call traces collected during Engine path execution. None for pipeline blocks.
    pub call_traces: Option<Vec<CallFrame>>,
}
```

`ExecutedBlock::new()` is unchanged; existing call sites use `..Default::default()` to populate the new field (`call_traces` defaults to `None`).

## Dependencies

```
engine/tree → evm/execution-types (existing)
engine/tree → revm-inspectors     (new; already in workspace)
engine/tree → alloy-rpc-types     (new; via evm/execution-types re-export or direct dep)
chain-state → evm/execution-types (existing)
```

No circular dependencies. `engine/tree` does not depend on `exex/` (FR-4 implementation: `has_exexs: bool` is computed in the node builder; no exex dependency is introduced in engine/tree).

## Error Handling and Edge Cases

| Scenario | Handling |
|----------|----------|
| `TracingInspector` allocation failure (OOM) | `executor.finish()` still completes normally; traces are lost but block validation is unaffected; returns `None` |
| Block with no transactions | `tx_traces` is an empty `Vec`; `BTreeMap` entry is present but empty |
| Reorg (`ChainReorged`) old chain | `blocks_to_chain` produces `None` traces; old chain carries no traces (FR-5) |
| Pipeline path | `execute_block` not invoked; `call_traces` is always `None` |
| Node restart | Traces are not written to WAL; `ChainCommitted` after restart carries no historical traces (Out of Scope) |

## Concurrency Safety

`TracingInspector` is created and consumed entirely within the `execute_block` call stack and is never shared across threads. No concurrency issues.

## Rollback Plan

This feature introduces no schema changes. The `call_traces` field carries `#[serde(skip)]`, making WAL round-trips transparent: WAL written by the old binary can be read by the new binary (new field defaults to `None`), and WAL written by the new binary can be read by the old binary (field is skipped). Rolling back means restoring the previous binary; no data migration is required.

## Test Plan

### Unit Tests

**T-1: `Chain::with_call_traces` setter/getter correctness**
- Scenario: construct a Chain, call `with_call_traces(btreemap)`
- Input: a Chain with 1 block, `BTreeMap<BlockNumber, Vec<CallFrame>>`
- Expected: `chain.call_traces()` returns the same data

**T-2: `Chain` serde skip — bincode round-trip**
- Scenario: serialize and deserialize a Chain that has `call_traces` set
- Input: a Chain where `call_traces` is `Some(...)`
- Expected: after deserialization, `call_traces()` returns `None` (traces are not written to WAL, Out of Scope)

**T-3: `Chain::call_traces()` returns `None` when not set**
- Scenario: construct via `Chain::new()` without calling `with_call_traces`
- Expected: `call_traces()` returns `None`

**T-4: `blocks_to_chain()` correctly injects traces**
- Scenario: construct 2 `ExecutedBlock`s, each with 1 `CallFrame`; call `blocks_to_chain`
- Expected: the returned Chain's `call_traces` contains entries keyed by the correct block numbers

**T-5: `blocks_to_chain()` produces no traces when all `ExecutedBlock`s have `None`**
- Scenario: all `ExecutedBlock.call_traces` are `None`
- Expected: `Chain::call_traces()` returns `None`

### Integration Tests

**T-6: call traces collected with complete nested call tree (FR-1)**
- Scenario: execute a block with 1 contract call (contract A internally calls contract B)
- Input: a transaction that calls contract A, which internally issues a CALL to contract B
- Expected: traces are not `None`; `traces[block_number][0].calls` is non-empty with at least 1 child `CallFrame`; child `CallFrame`'s `to` address equals contract B

**T-7: deeply nested call traces are complete (FR-3)**
- Scenario: execute a transaction with nested calls A → B → C
- Expected: root `CallFrame.calls` contains the full nested tree; each node (root and children) has non-empty `type`, `from`, `to`, `input`, `output`, `gas`, `gas_used`, `value` fields

**T-8: multiple transactions in a block each have an independent trace (FR-2)**
- Scenario: a block contains 3 transactions
- Expected: `traces[block_number].len() == 3`; each element corresponds to one transaction in block order

**T-9: reorg notification — old chain has no traces, new chain traces are valid (FR-4)**
- Scenario: trigger a chain reorg, producing a `ChainReorged` notification
- Expected: `old_chain.call_traces()` is `None`; `new_chain.call_traces()` is `Some` and `traces[block_number]` is non-empty with valid `from`/`to` on each `CallFrame`

**T-10: Pipeline path produces a Chain with no traces**
- Scenario: execute a block via the Pipeline path, producing a `ChainCommitted`
- Expected: `chain.call_traces()` is `None`

**T-11: ExEx can read call traces from notification (FR-2 end-to-end)**
- Scenario: register a mock ExEx subscribing to `ChainCommitted`; execute a block with 2 contract transactions via the Engine path
- Input: the notification received by the mock ExEx callback
- Expected: `chain.call_traces()` in the callback is `Some`; `traces[block_number].len() == 2`; each `CallFrame`'s `from`/`to` matches the corresponding transaction

## Out of Scope

Same as the requirements document Out of Scope section: Pipeline path, backfill, WAL persistence, opt-in subscription, opcode-level traces.

## ADRs

### ADR-1: Always inject `TracingInspector` rather than gating on `has_exexs`

**Status: Superseded** (previously: pass `has_exexs: bool` through call chain)

**Context:** The original design gated inspector injection on `has_exexs: bool` to avoid overhead when no ExEx is registered. This required threading the flag through 5 call levels and maintaining two separate transaction execution paths in `execute_block`.

**Decision:** Remove the gate; always inject `TracingInspector`. The `execute_transactions()` helper (used only in the no-inspector path) is deleted; a single inline tracing loop handles all cases.

**Rationale:** The overhead of `TracingInspector` with minimal config (`with_record_calls(true)` only) is expected to be within NFR-1's ≤15% budget even without ExEx. The simplification removes ~80 lines of code and eliminates the dual-path maintenance risk.

**Consequences:** Nodes without any ExEx registered now always pay the `TracingInspector` overhead. If profiling shows this exceeds NFR-1 on production hardware, a lightweight opt-in mechanism can be added in a future iteration (see `memory/project-exex-internal-txs-future.md`).

---

### ADR-2: Thread traces through an `ExecutedBlock` field rather than extending function return tuples

**Status: Accepted**

**Context:** There are multiple call layers between `execute_block` and `blocks_to_chain`. Two options: (a) extend return tuples at each call site; (b) add a field to `ExecutedBlock`.

**Decision:** Add `call_traces: Option<Vec<CallFrame>>` to `ExecutedBlock`, defaulting to `None`.

**Rationale:** `ExecutedBlock` already carries the block and its execution output; traces belong semantically to the block's execution result. A single field addition is more cohesive than changing multiple function signatures; changes are concentrated at the fill site (`execute_block`) and the read site (`blocks_to_chain`).

**Consequences:** `ExecutedBlock` is a public type; all construction sites are affected, but only require adding `..Default::default()` or `call_traces: None` — mechanical changes with no semantic impact.

---

### ADR-3: Use `CallFrame` (Geth callTracer format) rather than `TransactionTrace` (Parity format)

**Status: Accepted**

**Context:** `revm-inspectors` supports both Parity format (`TransactionTrace`) and Geth callTracer format (`CallFrame`). FR-3 requires output matching `debug_traceTransaction?tracer=callTracer`.

**Decision:** Use `CallFrame` from `alloy-rpc-types::eth::geth::call`.

**Rationale:** Directly satisfies FR-3; ExEx users are more familiar with the callTracer format (dominant in the Geth ecosystem); `alloy-rpc-types` is already in the workspace.

**Consequences:** Parity trace format is not supported (explicitly listed as Out of Scope).

## Known Limitations and Future Optimizations

### OPT-1: Replace `Vec<CallFrame>` with `Arc<Vec<CallFrame>>` to avoid deep clone in `blocks_to_chain()`

`ExecutedBlock.call_traces` and `Chain.call_traces` currently store `Vec<CallFrame>` directly.
In `blocks_to_chain()`, each block's `Vec<CallFrame>` is deep-cloned (recursive `CallFrame` tree)
to build the `BTreeMap` passed to `Chain::with_call_traces()`.

**Impact:**
- Normal chain-following (~12s/block): < 1ms per block, negligible.
- Backfill (~40-50 blocks/sec): ~20-25 MB/s of heap allocation, measurable under sustained load.

**Fix:** Change the inner type to `Arc<Vec<CallFrame>>` in both `ExecutedBlock` and `Chain`.
`blocks_to_chain()` would then clone only the `Arc` pointer (8 bytes + atomic increment) instead
of the full tree. The change touches only our newly-added code; no existing callers are affected.

**Deferred because:** negligible in the primary use case (live chain following). Revisit if
profiling shows backfill memory pressure.

### OPT-2: `Chain::append_chain` silently drops `call_traces` from the appended chain

`Chain::append_chain` merges blocks, execution outcome, and trie data from `other` into `self`,
but does not merge `other.call_traces`. Traces from the appended chain are silently dropped.

**Impact:**
- The Engine path does not call `append_chain`; all Engine-path chains are built via
  `blocks_to_chain` → `with_call_traces`, so this has no effect on current functionality.
- Future callers who use `append_chain` to join Engine-path chains will silently lose traces
  for the appended portion.

**Fix:** In `append_chain`, after extending blocks/outcome/trie_data, add:
```rust
#[cfg(feature = "traces")]
if let Some(other_traces) = other.call_traces {
    self.call_traces.get_or_insert_with(BTreeMap::new).extend(other_traces);
}
```

**Deferred because:** no current caller is affected. Revisit if `append_chain` is ever used
on Engine-path chains.

## Changelog

- 2026-05-17: Initial version
- 2026-05-18: Clarified inline tracing loop architecture; confirmed revm-inspectors API (`geth_builder()`, `fuse()`, `geth_call_traces()`); corrected `gas_used` to `tx_gas_used()` only (state_gas_used must not be included)
- 2026-05-18: Fixed inspector access: `alloy_evm::Evm` trait exposes `inspector_mut()` via `components_mut()`; updated tracing loop to use `executor.evm_mut().inspector_mut()` instead of a separately held `&mut TracingInspector`; inspector is now passed by value (owned) to `evm_with_env_and_inspector`
- 2026-05-18: Removed `has_exexs` gate; `TracingInspector` now always injected; `execute_transactions()` helper deleted; ADR-1 superseded; FR-4 removed from requirements; T-6 removed from test plan; integration test numbers updated
