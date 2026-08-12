# ExEx Internal Transactions Support (v2.4.0 Port)

## Background

Reth's ExEx (Execution Extension) currently receives chain data via `ExExNotification` — blocks,
transactions, receipts, and state diffs — but does not include internal transactions (call traces).
Call traces are currently only generated on-demand at the RPC layer (`debug_traceTransaction` /
`debug_traceBlock`) and cannot be subscribed to by ExEx.

This document describes porting the ExEx call traces feature (previously implemented on
`dev-exex-internal-txs-v2.3.0`) to the `dev-exex-internal-txs-v2.4.0` branch based on reth v2.4.0.

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

**Exception paths:**
- Given: a block with 0 transactions
- When: the Engine path executes the block
- Then: `call_traces` for this block is an empty `Vec<CallFrame>` (the block still appears in
  the traces map with an empty list)

- Given: a transaction that reverts during execution
- When: the Engine path executes the block containing this transaction
- Then: the call trace for the reverted transaction is still collected, including revert reason
  in the output field

- Given: the tracing inspector encounters an internal error
- When: this scenario is not expected to occur; `revm-inspectors` guarantees no panics during
  trace collection under normal operation
- Then: not applicable — no defensive handling required in this system

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

### NFR-1 (Should): Performance overhead for nodes with ExEx
- Additional CPU overhead on the Engine path <= 15% relative to the no-inspector baseline
- No hard limit on per-block trace memory; controlled via minimal `TracingInspectorConfig`
  (opcode/memory/stack recording disabled)
- Note: this iteration does not include benchmark verification; performance will be observed
  in production. The feature was previously validated on v2.3.0.

### NFR-2 (Must): Backward compatibility
- The new field on `Chain` is `Option` typed and defaults to `None`; existing ExEx code compiles
  without modification
- `ExExNotification` structure is unchanged
- The existing `execute_transactions` function signature and behavior remain unchanged

## Assumptions & Constraints

1. The Engine path (newPayload) is the only real-time block execution path that produces call
   traces. Pipeline (historical sync) and Backfill paths do not collect traces.
2. ExEx instances are responsible for handling `call_traces = None` gracefully (e.g., for
   historical blocks, BAL-executed blocks, or reorg old-chain blocks).
3. Traces are ephemeral — they are not persisted to WAL or any durable storage. A node restart
   loses all previously collected traces.

## Dependencies

- `revm-inspectors` (workspace): provides `TracingInspector` and `TracingInspectorConfig`
- `alloy-evm`: `ConfigureEvm::evm_with_env_and_inspector` creates EVM with custom inspector;
  `BlockExecutor::execute_transaction` returns `GasOutput` for trace gas accounting
- `alloy-rpc-types-trace`: provides `CallFrame` type

## Verification Criteria (Executable)

1. `cargo +nightly fmt --all --check` passes
2. `cargo +nightly clippy --workspace --all-features` passes (or scoped to modified crates)
3. `cargo nextest run -p reth-chain-state -p reth-execution-types -p reth-engine-tree` passes
4. Unit tests cover:
   - `test_blocks_to_chain_injects_traces`
   - `test_blocks_to_chain_no_traces_when_none`
   - `test_blocks_to_chain_partial_traces_for_mixed_blocks`
   - `test_to_chain_notification_commit_includes_traces`
   - `test_to_chain_notification_reorg_traces`
5. Sequential execution path blocks have `Chain.call_traces = Some(...)`
6. BAL execution path blocks have `Chain.call_traces = None`

## Change History

- 2026-06-24: Initial version — ported from v2.2.0 spec to v2.3.0
- 2026-08-12: Ported to v2.4.0 — no functional changes, adapted to v2.4.0 code structure
  (DeferredTrieData → LazyTrieData, Arc<RecoveredBlock>, new payload validation pipeline).
  NFR-1 降级为 Should（本次不做 benchmark）；补充异常路径验收标准；新增 Assumptions &
  Constraints 章节。
