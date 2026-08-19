# ExEx Internal Transactions (Call Traces) — Requirements (v2.5.0)

## Background

Reth ExEx (Execution Extension) plugins receive `ExExNotification` when canonical chain advances or
reorgs. Currently notifications carry blocks and execution outcomes (receipts, state changes) but
**not** internal transaction traces (call frames). DeFi analytics, MEV detection, and forensic
tools need per-transaction call trees without running a separate `debug_traceTransaction` RPC call
against every transaction.

This document specifies the requirements for injecting call traces into the ExEx notification
pipeline for reth v2.5.0.

## Goal

Provide per-transaction call traces (Geth `callTracer` format) to ExEx plugins via the existing
notification mechanism, with zero impact on nodes that do not use this feature.

## Functional Requirements

| ID | Priority | Description |
|----|----------|-------------|
| FR-1 | Must | During Engine path block execution, the system shall collect a call trace (call frame tree) for each user transaction in the block. |
| FR-2 | Must | Call traces shall be propagated through `ExecutedBlock` → `Chain` → `ExExNotification` so that ExEx plugins can access them via `chain.call_traces()`. |
| FR-3 | Must | The call trace format shall be `CallFrame` from `alloy-rpc-types-trace`, identical to Geth's `callTracer` output format. |
| FR-4 | Must | The feature shall be gated behind a Cargo feature flag (`traces`) so that nodes not using ExEx call traces incur zero compile-time or runtime cost. |
| FR-5 | Must | System calls (beacon root update, etc.) executed during `apply_pre_execution_changes` shall NOT appear in any transaction's call trace. |
| FR-6 | Must | Each transaction's trace shall be isolated — no bleed between consecutive transactions. |
| FR-7 | Should | Reverted transactions shall still have their call trace recorded (with error info in the frame). |

## Non-Functional Requirements

| ID | Priority | Description |
|----|----------|-------------|
| NFR-1 | Should | Runtime overhead when the `traces` feature is enabled shall be < 20% wall-clock time on average mainnet blocks. |
| NFR-2 | Must | When the `traces` feature is NOT enabled (default), there shall be zero runtime overhead — no inspector allocation, no trait object indirection. |
| NFR-3 | Must | The existing `execute_transactions` code path shall remain unmodified to minimize merge conflict risk with upstream. |
| NFR-4 | Should | Memory: call traces are transient (in-memory only, not persisted). No size cap this iteration. |

## Assumptions and Constraints

1. Only the Engine path (live sync via CL) produces call traces. Pipeline sync and WAL recovery do NOT produce traces.
2. BAL (Block Access List) parallel execution path does NOT produce traces (returns `None`).
3. `TracingInspectorConfig::none()` records call tree structure but disables opcode/memory/stack recording.
4. Call traces are not serialized (serde skip) — WAL recovery and bincode roundtrips lose traces by design.
5. Reorg notifications: old-chain blocks that were previously executed via Engine path retain their traces if present; ExEx consumers distinguish old vs new via `ChainReorged { old, new }` variant.

## Out of Scope

- Trace persistence (database storage)
- Trace size caps or memory budgeting
- Pipeline path trace collection
- Opt-in subscription mechanism (ExEx declares interest)
- `debug_traceBlock` RPC integration
- Performance benchmarking (deferred to future iteration)
- BAL path trace collection

## Dependencies

| Dependency | Role |
|-----------|------|
| `alloy-rpc-types-trace` | Provides `CallFrame` type |
| `revm-inspectors` | Provides `TracingInspector` and `TracingInspectorConfig` |
| `alloy-evm` | Provides `GasOutput::tx_gas_used()` for trace extraction |

## Acceptance Criteria

### AC-1: Trace collection (FR-1, FR-5, FR-6)
- **Given** a block with N user transactions executed via Engine path
- **When** execution completes
- **Then** `ExecutedBlock.call_traces` contains `Some(Vec<CallFrame>)` with exactly N elements, one per transaction in order; no system call traces are present.

### AC-2: Trace propagation (FR-2)
- **Given** `ExecutedBlock` instances with `call_traces = Some(...)`
- **When** `blocks_to_chain` constructs a `Chain`
- **Then** `chain.call_traces()` returns `Some(BTreeMap)` keyed by block number, with matching traces.

### AC-3: Format compatibility (FR-3)
- **Given** a collected `CallFrame`
- **When** serialized to JSON
- **Then** the output matches the schema of Geth's `callTracer` response (same type used by `debug_traceTransaction`).

### AC-4: Feature gating (FR-4, NFR-2)
- **Given** a build without the `traces` feature
- **When** compiling
- **Then** no `alloy-rpc-types-trace` or `revm-inspectors` dependency is pulled; `ExecutedBlock` and `Chain` do not have `call_traces` fields.

### AC-5: Isolation (FR-6)
- **Given** a block with 2+ transactions where tx[0] has nested calls
- **When** traces are collected
- **Then** tx[1]'s trace does not contain any calls from tx[0].

### AC-6: Reverted transaction (FR-7)
- **Given** a transaction that reverts
- **When** its trace is collected
- **Then** the `CallFrame` is present with error information.

### AC-7: BAL path (Constraint 2)
- **Given** a block executed via the BAL parallel path
- **When** execution completes
- **Then** `call_traces` is `None`.

### AC-8: Empty block
- **Given** a block with 0 user transactions
- **When** executed via Engine path
- **Then** `call_traces` is `Some(vec![])` (empty vector, not None).

### AC-9: Existing path unchanged (NFR-3)
- **Given** the original `execute_transactions` function
- **When** comparing before and after
- **Then** it is unmodified (new logic lives in a separate `execute_transactions_traced` function).

## Verification Commands

```bash
# AC-4: Feature gating — compile without traces feature, no extra deps
cargo check -p reth-execution-types
cargo check -p reth-chain-state

# AC-1, AC-2, AC-5, AC-6, AC-7, AC-8: Unit tests
cargo nextest run -p reth-execution-types -p reth-chain-state -p reth-engine-tree

# AC-9: Diff check — original execute_transactions unchanged
git diff v2.5.0 -- crates/engine/tree/src/tree/payload_validator.rs | grep "^-" | grep -v "^---" | grep "execute_transactions"
# Expected: no removals from the original function
```
