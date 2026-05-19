# ExEx Internal Transactions Support

## Background

Reth's ExEx (Execution Extension) currently receives chain data via `ExExNotification` — blocks,
transactions, receipts, and state diffs — but does not include internal transactions (call traces).
Call traces are currently only generated on-demand at the RPC layer (`debug_traceTransaction` /
`debug_traceBlock`) and cannot be subscribed to by ExEx.

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
- When: the Engine path executes the block
- Then: call traces are fully collected for every transaction, including the complete nested call tree

### FR-2 (Must): Push call traces via ExExNotification

The system must attach the collected call traces to the `Chain` data structure inside
`ExExNotification::ChainCommitted`. ExEx instances access traces via a new field on `Chain`.

**Acceptance criteria:**
- Given: an ExEx is registered and subscribed to notifications
- When: the Engine path executes a new block and emits a `ChainCommitted` notification
- Then: the `Chain` in the notification contains call traces for all transactions in the block,
  organized as `BlockNumber → Vec<CallTrace>`

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
- Then: `old.call_traces` is `None`; `new.call_traces` contains traces from the newly executed block

## Out of Scope

| Item | Notes |
|------|-------|
| Pipeline (historical sync) path | Not changed; `call_traces` is `None` for historical blocks |
| Backfill path | No traces when ExEx replays historical blocks on first startup |
| WAL persistence | Traces are not written to WAL; lost on node restart |
| Opt-in subscription mechanism | All ExEx instances receive traces uniformly; per-ExEx opt-in is a future improvement |
| Opcode-level traces | No `structLogs`, memory, or stack collection |
| Parity trace format | Only Geth callTracer format is supported |

## Non-Functional Requirements

### NFR-1 (Must): Performance overhead for nodes with ExEx
- Additional CPU overhead on the Engine path ≤ 15% relative to the no-inspector baseline
- No hard limit on per-block trace memory; controlled via minimal `TracingInspectorConfig`
  (opcode/memory/stack recording disabled)

### NFR-2 (Must): Backward compatibility
- The new field on `Chain` is `Option` typed and defaults to `None`; existing ExEx code compiles
  without modification
- `ExExNotification` structure is unchanged

## Dependencies

- `revm-inspectors` v0.39.0 (already in workspace): provides `TracingInspector` and
  `TracingInspectorConfig`
- `alloy-evm` v0.34.0: `BlockExecutor::finish()` returns `(Evm, BlockExecutionResult)`, allowing
  the inspector to be retrieved after execution

## Future Improvements

- **Opt-in subscription**: allow each ExEx to declare whether it needs call traces; only inject
  inspector when at least one ExEx requires traces. See
  `docs/spec/exex-internal-txs-future.md` (to be created when scoped).
- **Minimal custom inspector**: implement a lightweight `MinimalCallTracer` with only
  call/create/selfdestruct hooks to reduce per-frame memory by ~70% compared to `TracingInspector`.
  Defer until profiling confirms `TracingInspector` is a bottleneck.

## Change History

- 2026-05-17: Initial version
- 2026-05-18: Removed FR-4 (zero overhead when no ExEx registered); `TracingInspector` is now always injected regardless of ExEx registration; FR-5 renumbered to FR-4
