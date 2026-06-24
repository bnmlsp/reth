# ExEx Internal Transactions Support (v2.3.0 Port)

## Background

Reth's ExEx (Execution Extension) currently receives chain data via `ExExNotification` — blocks,
transactions, receipts, and state diffs — but does not include internal transactions (call traces).
Call traces are currently only generated on-demand at the RPC layer (`debug_traceTransaction` /
`debug_traceBlock`) and cannot be subscribed to by ExEx.

This document describes porting the ExEx call traces feature (originally implemented on
`dev-exex-internal-txs` against v2.2.0) to the `dev-exex-internal-txs-v2.3.0` branch based on
reth v2.3.0.

## Goal

When the Engine (newPayload) path executes a new block, collect call traces and push them to ExEx
subscribers in real time via `ExExNotification`, so ExEx instances can access internal transaction
data out of the box.

## Functional Requirements

### FR-1 (Must): Collect call traces during Engine path block execution

When the node executes a new block via the Engine API (`newPayload`), the system must collect call
traces for all transactions in the block by injecting an inspector during execution.

**Acceptance criteria:**
- Given: the node is following the chain tip and receives a new block with N transactions
- When: the Engine path executes the block via the sequential execution path (`execute_block`)
- Then: call traces are fully collected for every transaction, including the complete nested call
  tree

### FR-2 (Must): Push call traces via ExExNotification

The system must attach the collected call traces to the `Chain` data structure inside
`ExExNotification::ChainCommitted`. ExEx instances access traces via a new field on `Chain`.

**Acceptance criteria:**
- Given: an ExEx is registered and subscribed to notifications
- When: the Engine path executes a new block and emits a `ChainCommitted` notification
- Then: the `Chain` in the notification contains call traces for all transactions in the block,
  organized as `BlockNumber -> Vec<CallFrame>`

### FR-3 (Must): Data format matches Geth callTracer

The call trace format must be consistent with the output of `debug_traceTransaction` using the
`callTracer` tracer. Fields include `type`, `from`, `to`, `value`, `gas`, `gasUsed`, `input`,
`output`, and `calls[]` for nested call trees.

**Acceptance criteria:**
- Given: a transaction containing multiple levels of nested calls
- When: an ExEx retrieves the call traces from the notification
- Then: the data structure matches the field layout returned by
  `debug_traceTransaction?tracer=callTracer` for the same transaction

### FR-4 (Must): Old chain in reorg/revert carries no traces

For `ChainReorged` and `ChainReverted` notifications, the `call_traces` field of the old chain
must be `None`.

**Acceptance criteria:**
- Given: a chain reorg occurs, with the old chain containing historical blocks
- When: an ExEx receives a `ChainReorged` notification
- Then: `old.call_traces` is `None`; `new.call_traces` contains traces from the newly executed
  blocks

## Out of Scope

| Item | Notes |
|------|-------|
| Pipeline (historical sync) path | Not changed; `call_traces` is `None` for historical blocks |
| Backfill path | No traces when ExEx replays historical blocks on first startup |
| WAL persistence | Traces are not written to WAL; lost on node restart |
| Opt-in subscription mechanism | All ExEx instances receive traces uniformly; per-ExEx opt-in is a future improvement |
| Opcode-level traces | No `structLogs`, memory, or stack collection |
| Parity trace format | Only Geth callTracer format is supported |
| BAL parallel execution path (EIP-7928) | When `execute_block_bal` is used, `call_traces = None`; BAL is not yet enabled on mainnet and parallel tracing requires per-worker inspectors with ordered aggregation — deferred to a future iteration |

## Non-Functional Requirements

### NFR-1 (Must): Performance overhead for nodes with ExEx
- Additional CPU overhead on the Engine path <= 15% relative to the no-inspector baseline
- No hard limit on per-block trace memory; controlled via minimal `TracingInspectorConfig`
  (opcode/memory/stack recording disabled)

### NFR-2 (Must): Backward compatibility
- The new field on `Chain` is `Option` typed and defaults to `None`; existing ExEx code compiles
  without modification
- `ExExNotification` structure is unchanged
- The existing `execute_transactions` function signature and behavior remain unchanged

## Dependencies

- `revm-inspectors` v0.40.1 (workspace): provides `TracingInspector` and `TracingInspectorConfig`
- `alloy-evm` v0.36.0: `ConfigureEvm::evm_with_env_and_inspector` creates EVM with custom
  inspector; `BlockExecutor::execute_transaction` returns `GasOutput` for trace gas accounting
- `alloy-rpc-types-trace`: provides `CallFrame` type

## Verification Criteria (Executable)

1. `cargo +nightly fmt --all --check` passes
2. `cargo +nightly clippy --workspace --all-features` passes
3. `cargo nextest run -p reth-chain-state -p reth-execution-types -p reth-engine-tree` passes
4. Unit tests ported from the v2.2.0 branch pass:
   - `test_blocks_to_chain_injects_traces`
   - `test_blocks_to_chain_no_traces_when_none`
   - `test_blocks_to_chain_partial_traces_map_for_mixed_blocks`
   - `test_blocks_to_chain_suppresses_traces_when_include_false`
5. Sequential execution path blocks have `Chain.call_traces = Some(...)`
6. BAL execution path blocks have `Chain.call_traces = None`

## Change History

- 2026-06-24: Initial version — ported from v2.2.0 spec with BAL out-of-scope addition and
  dependency version updates
