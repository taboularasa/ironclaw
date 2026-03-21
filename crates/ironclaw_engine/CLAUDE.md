# IronClaw Engine Crate

Unified thread-capability-CodeAct execution model. Replaces ~10 separate abstractions (Session, Job, Routine, Channel, Tool, Skill, Hook, Observer, Extension, LoopDelegate) with 5 primitives.

## Full Architecture Plan

See `docs/plans/2026-03-20-engine-v2-architecture.md` for the 8-phase roadmap.

## Five Primitives

| Primitive | Purpose | Replaces |
|-----------|---------|----------|
| **Thread** | Unit of work with lifecycle, parent-child tree, capability leases | Session + Job + Routine + Sub-agent |
| **Step** | Unit of execution (one LLM call + its action executions) | Agentic loop iteration + tool calls |
| **Capability** | Unit of effect (actions + knowledge + policies) | Tool + Skill + Hook + Extension |
| **MemoryDoc** | Unit of durable knowledge (summaries, lessons, playbooks) | Workspace memory blobs |
| **Project** | Unit of context (scopes memory, threads, missions) | Flat workspace namespace |

## Build & Test

```bash
cargo check -p ironclaw_engine
cargo clippy -p ironclaw_engine --all-targets -- -D warnings
cargo test -p ironclaw_engine
```

## Module Map

```
src/
├── lib.rs                # Public API, re-exports
├── types/                # Core data structures (no async, no I/O)
│   ├── thread.rs         # Thread, ThreadId, ThreadState (state machine), ThreadType, ThreadConfig
│   ├── step.rs           # Step, StepId, LlmResponse, ActionCall, ActionResult, TokenUsage
│   ├── capability.rs     # Capability, ActionDef, EffectType, CapabilityLease, PolicyRule
│   ├── memory.rs         # MemoryDoc, DocId, DocType (Summary/Lesson/Playbook/Issue/Spec/Note)
│   ├── project.rs        # Project, ProjectId
│   ├── event.rs          # ThreadEvent, EventKind (16 variants for event sourcing)
│   ├── message.rs        # ThreadMessage, MessageRole
│   ├── provenance.rs     # Provenance enum (User/System/ToolOutput/LlmGenerated/etc.)
│   └── error.rs          # EngineError, ThreadError, StepError, CapabilityError
├── traits/               # External dependency abstractions (host implements these)
│   ├── llm.rs            # LlmBackend trait
│   ├── store.rs          # Store trait (18 CRUD methods)
│   └── effect.rs         # EffectExecutor trait
├── capability/           # Capability management
│   ├── registry.rs       # CapabilityRegistry — register/get/list capabilities
│   ├── lease.rs          # LeaseManager — grant/check/consume/revoke/expire leases
│   └── policy.rs         # PolicyEngine — deterministic effect-level allow/deny/approve
├── runtime/              # Thread lifecycle management
│   ├── manager.rs        # ThreadManager — spawn, stop, inject messages, join threads
│   ├── tree.rs           # ThreadTree — parent-child relationships
│   └── messaging.rs      # ThreadSignal, ThreadOutcome, signal channels
├── executor/             # Step execution
│   ├── loop_engine.rs    # ExecutionLoop — core loop replacing run_agentic_loop()
│   ├── structured.rs     # Tier 0: structured tool call execution
│   ├── context.rs        # Context builder (messages + actions from leases)
│   └── intent.rs         # Tool intent nudge detection
├── memory/               # Memory document system
│   ├── store.rs          # MemoryStore — project-scoped doc CRUD
│   └── retrieval.rs      # RetrievalEngine — context building (stub, Phase 4)
└── reflection/           # Post-thread reflection (stub, Phase 4)
    └── mod.rs
```

## Thread State Machine

```
Created → Running → Waiting → Running (resume)
                  → Suspended → Running (resume)
                  → Completed → Reflecting → Done
                  → Failed
```

Validated by `ThreadState::can_transition_to()`. Terminal states: `Done`, `Failed`.

## External Trait Boundaries

The engine defines three traits that the host crate implements:

| Trait | Purpose | Host wraps |
|-------|---------|------------|
| `LlmBackend` | `complete(messages, actions, config) -> LlmOutput` | `LlmProvider` |
| `Store` | Thread/Step/Event/Project/Doc/Lease CRUD | `Database` (PostgreSQL + libSQL) |
| `EffectExecutor` | `execute_action(name, params, lease, ctx) -> ActionResult` | `ToolRegistry` + `SafetyLayer` |

## Execution Loop

`ExecutionLoop::run()` mirrors `run_agentic_loop()`:

1. Check signals (Stop, InjectMessage) via `mpsc::Receiver`
2. Build context (messages + available actions from active leases)
3. Call LLM via `LlmBackend::complete()`
4. If text: check tool intent nudge, return if final response
5. If action calls: for each call, find lease → check policy → consume use → execute via `EffectExecutor` → record result
6. Record Step, emit ThreadEvents
7. Repeat until: text response, stop signal, max iterations, or approval needed

## Capability Leases

Threads don't have static permissions. They receive **leases** — scoped, time-limited, use-limited grants:

```rust
CapabilityLease {
    thread_id, capability_name, granted_actions,
    expires_at: Option<DateTime>,  // time-limited
    max_uses: Option<u32>,         // use-limited
    revoked: bool,
}
```

The `PolicyEngine` evaluates actions against leases deterministically: `Deny > RequireApproval > Allow`.

## Effect Types

Every action declares its side effects. The policy engine uses these for allow/deny:

```
ReadLocal, ReadExternal, WriteLocal, WriteExternal,
CredentialedNetwork, Compute, Financial
```

## Key Design Decisions

1. **No dependency on main `ironclaw` crate** — clean separation, testable in isolation
2. **No safety logic** — sanitization/leak detection is applied at the adapter boundary (`EffectExecutor` impl)
3. **Event sourcing from day one** — every thread records a complete event log via `ThreadEvent`
4. **Tier 0 only (MVP)** — structured tool calls. CodeAct (Tier 1-3) added in Phase 3
5. **Engine owns its message type** — `ThreadMessage` is simpler than `ChatMessage`; bridge adapters handle conversion

## Code Style

Follows the main crate's conventions from `/CLAUDE.md`:
- No `.unwrap()` or `.expect()` in production code (tests are fine)
- `thiserror` for error types
- Map errors with context
- Prefer strong types over strings (newtypes for IDs)
- All I/O is async with tokio
- `Arc<T>` for shared state, `RwLock` for concurrent access
