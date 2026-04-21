# Agent Re-Core: Minimal Loop for a Smarter ZeroClaw

**Date:** 2026-04-21
**Status:** Draft — design approved, awaiting spec review
**Target model:** MiniMax M2 (default); architecture agnostic

## Problem

The ZeroClaw agent feels dumb across all axes simultaneously:

- Gives up after tool failures instead of trying alternatives
- Loses track of the task mid-run, repeats itself, forgets earlier steps
- Picks the wrong tool when the right one is obvious
- Stops after one step when the task needs several
- Does not learn from mistakes within a session

A previous refactor introduced a memory leak in the `brain.db` / memory subsystem
that grew RSS unbounded — 4 GB, 6 GB, 8 GB — matching whatever the container was
given. This is a hard constraint: the binary must stay ~10–20 MB and RSS must
stay flat across long sessions so ZeroClaw can run 4–6 agent containers per
customer.

## Diagnosis

The intelligence problem is not the model and not the absence of a reasoning
framework (ReAct, tree-of-thought, etc.). It is scaffolding.

Evidence in the current tree:

- `src/agent/loop_.rs` is **10,126 lines**; `agent.rs` is **1,924 lines**.
- Around the core loop sit: `classifier`, `context_analyzer`,
  `context_compressor` (861 lines), `dispatcher`, `eval`, `history_pruner`,
  `loop_detector` (696 lines), `memory_loader`, `microcompactor`, `personality`,
  `thinking`.
- `src/memory/` contains: `dreaming`, `lucid`, `consolidation`, `decay`,
  `conflict`, `hygiene`, `importance`, `knowledge_graph`, `procedural`,
  `outcomes`, `response_cache`, `embeddings`, `vector`, `qdrant`, `chunker`,
  `customer_model`.

Every one of these subsystems edits the model's view of reality — compressing
history, rewriting tool errors, classifying intent ahead of the model,
injecting personality, pruning what the model just did. Hermes, Claude Code,
and the local Claurst port are all dramatically smaller and behave smarter
because they do not do this. They hand the model a clean transcript, a good
tool list, and a loop that does not quit early.

What actually produces the feeling of intelligence in those agents:

1. The loop does not bail early. It runs until the model stops calling tools
   or hits a hard iteration budget.
2. Tool errors come back verbatim (truncated only from the head, never the
   tail — the tail is where the real error is).
3. Context is a transcript, not a summary. No compression, no microcompaction.
4. One system prompt, plainly written. Not assembled from many injectors.
5. Tool descriptions are treated as first-class prompt engineering.

Cross-session "learning" via a background memory system is the class of
feature that caused the leak. It will not be re-added. Memory becomes an act
the model performs via a tool, not a system that runs.

## Comparison to upstream `zeroclaw-labs/zeroclaw`

Upstream shares the same shape: orchestration loop with message classifier,
memory loader, lifecycle hooks, and a large memory subsystem. It is not
meaningfully cleaner. Mirroring upstream would not solve the problem.

This design improves on upstream by:

- Removing the classifier, compressor, microcompactor, history_pruner,
  context_analyzer, loop_detector heuristics, personality injector,
  memory_loader, and thinking module from the hot path.
- Replacing the 15K-line memory subsystem (dreaming, lucid, consolidation,
  decay, knowledge_graph, embeddings, vector, qdrant, response_cache,
  chunker, customer_model, hygiene, importance, procedural, outcomes,
  conflict) with a single markdown file and two tools the model can call.
- Forbidding background tasks between turns by construction, and adding a
  CI test that asserts flat RSS across 500 simulated turns.

## Goals

1. Agent that persistently attempts the user's request, reacts to failures,
   and chains multi-step work without giving up.
2. Binary stays in the 10–20 MB envelope. RSS stays flat across long-running
   sessions.
3. Existing infrastructure (providers, tools, channels, tunnel, daemon, auth,
   security, approval, observability, cost, mcp_server, tui, peripherals,
   hardware, multimodal) is preserved.
4. The legacy agent stays runnable behind a flag until the new core is at
   parity, then it is deleted.

## Non-Goals

- No new reasoning frameworks (ReAct, ToT, reflection loops). The plain tool
  loop is ReAct when the model is given tools.
- No cross-session learning system. No embeddings. No vector DB. No
  consolidation or background dreaming.
- No rewrite of providers, tools, channels, tunnel, daemon, auth, security,
  approval, observability, cost, mcp_server, tui, peripherals, hardware, or
  multimodal.
- No model change. Default remains MiniMax M2.

## Design

### New module layout

Target ~1,500 lines total across these files:

```
src/agent/core/
  mod.rs          // public entry: run_turn, run_session
  loop.rs         // the tool-use loop
  prompt.rs       // single plainly-written system prompt
  tools.rs        // thin wrapper over src/tools registry
  errors.rs       // ToolResult shape + tail-preserving truncation
  history.rs      // append-only transcript with context-limit fallback
  memory.rs       // remember() / recall() tools over a markdown file
```

### The loop

Pseudocode:

```
loop {
    if iterations >= max_iterations { break; }
    let stream = provider.chat(history, tools, system_prompt);
    let (text, tool_calls) = consume(stream);
    history.push_assistant(text, tool_calls);
    if tool_calls.is_empty() { break; }
    for call in tool_calls {
        let result = tools.execute(call).await;
        history.push_tool_result(call.id, result);
    }
    iterations += 1;
}
```

No classifier before the call. No compressor after. No microcompactor between
turns. No loop_detector second-guessing the model. `max_iterations` is a
safety cap (default 50) — it is not a "this looks complete" heuristic.

### Tool-error contract

Every tool returns a `ToolResult`:

```rust
pub struct ToolResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
}
```

Rules:

- Errors are passed to the model unmodified in content.
- If truncation is needed, truncate from the **head**, keep the tail, and
  set `truncated = true` with a clear prefix note. The tail is where the
  actual error lives.
- No tool wrapper rewrites, summarizes, or "helpfully" reformats a failure.

### History / context

- Transcript is append-only for the duration of a turn.
- Trim only when the provider reports a context-limit error, and only by
  dropping the oldest complete tool-call/tool-result pair (never mid-turn,
  never summarizing).
- Session persistence uses the existing interactive session save/load; the
  new core consumes the same format so sessions remain resumable.

### System prompt

One file, ~100 lines max. Contains:

- Today's date
- Current working directory and OS
- Available tool categories (one sentence each)
- How to call tools
- How to signal completion (stop calling tools)
- Nothing else. No personality. No classifier hints. No memory dumps.

### Memory, redesigned

Two tools, backed by one markdown file per project at
`$ZEROCLAW_HOME/memory/<project>.md`:

- `remember(fact: string, tags?: string[])` — appends a line with ISO
  timestamp and tags.
- `recall(query: string, limit?: usize)` — `grep -i` over the file,
  returns top `limit` lines (default 20) with their timestamps.

No embeddings. No vector search. No background writer. No consolidation.
Memory is an explicit model action, not an ambient system. If a user wants
richer retrieval later, it gets added as another tool — never as something
that runs between turns.

### Tool-description pass

MiniMax M2's agentic behavior is highly sensitive to tool descriptions.
Every tool in `tool_descriptions/` and `tools/` gets rewritten to the
following schema:

- One-sentence purpose
- When to use it vs. when not to
- Argument schema with a realistic example
- What a successful result looks like
- What a failure looks like (so the model can react correctly)

This is the single biggest "wrong tool chosen" lever and it costs no
runtime complexity.

### Feature flag and rollout

- Config key `agent.core = "legacy" | "minimal"`, default `"legacy"` until
  eval parity is met.
- CLI flag `--core=minimal` overrides for ad-hoc testing.
- Existing `src/agent/eval.rs` is extended into a task-suite runner that
  executes the same ~15–20 tasks against both cores and reports per-task
  pass/fail, iterations used, and tokens spent.
- Task suite covers the four failure modes explicitly:
  - Diligence after tool failure (tool returns error; model must retry
    with a different approach).
  - Multi-step chaining (≥4 dependent tool calls without the user
    prompting again).
  - Wrong-tool recovery (model picks a plausible-but-wrong tool first;
    must pivot after seeing the result).
  - Long context retention (task that references a fact established 15+
    turns earlier).
- Promotion rule: `minimal` becomes default when it wins or ties on ≥80%
  of tasks **and** uses equal-or-fewer tokens on the median. Then the
  legacy code and heavy memory modules are deleted.

### What gets deleted at parity

From `src/agent/`:

- `loop_.rs` (10,126 lines)
- `agent.rs` (1,924 lines)
- `classifier.rs`, `context_analyzer.rs`, `context_compressor.rs`,
  `microcompactor.rs`, `history_pruner.rs`, `loop_detector.rs`,
  `personality.rs`, `thinking.rs`, `memory_loader.rs`

From `src/memory/`:

- `dreaming.rs`, `lucid.rs`, `consolidation.rs`, `decay.rs`,
  `knowledge_graph.rs`, `hygiene.rs`, `importance.rs`, `procedural.rs`,
  `outcomes.rs`, `conflict.rs`, `response_cache.rs`, `embeddings.rs`,
  `vector.rs`, `qdrant.rs`, `chunker.rs`, `customer_model.rs`

What stays in `src/memory/`: the `Memory` trait and a thin markdown-backed
implementation consumed by the `remember` / `recall` tools. Everything else
is removed with its tests and call-sites.

### What is preserved untouched

`providers/`, `tools/`, `channels/`, `tunnel/`, `daemon/`, `auth/`,
`security/`, `approval/`, `observability/`, `cost/`, `mcp_server/`, `tui/`,
`peripherals/`, `hardware/`, `multimodal/`, `skillforge/`, `skills/`,
`sop/`, `plugins/`, `hooks/`, `rag/`, `nodes/`, `routines/`, `runtime/`,
`verifiable_intent/`, `trust/`, `gateway/`, `integrations/`, `cron/`,
`health/`, `heartbeat/`, `doctor/`, `onboard/`, `hands/`, `service/`,
`commands/`.

### Memory-leak guardrails (binding rules for the new core)

1. No `LazyLock<Mutex<HashMap<…>>>` / `LazyLock<Mutex<Vec<…>>>` / similar
   module-level growing caches inside `src/agent/core/`. Enforced by a
   grep-based lint in CI.
2. No `tokio::spawn` of long-lived loops from the agent core. Work happens
   inside a turn or not at all. Enforced by the same lint.
3. Per-session state lives in one struct owned by the loop and is dropped
   when the session ends. No static session registries.
4. New integration test `tests/agent_rss.rs` runs 500 turns against a
   mocked provider and asserts RSS growth stays under a threshold
   (initial target: 20 MB across the run). Fails CI if it regresses.
5. The full `src/memory/` audit listed above lands in the same PR as the
   deletion step — no dead code left behind.

## Data Flow

```
user_input
   │
   ▼
[session_load]──► history
   │                │
   │                ▼
   │        ┌───────────────┐
   └──────► │   core::loop  │
            │               │
            │  provider.chat│──► stream ──► assistant text + tool calls
            │       │       │
            │       ▼       │
            │  tools.exec   │──► ToolResult (verbatim)
            │       │       │
            │       ▼       │
            │  history.push │
            └───────┬───────┘
                    │ (no tool calls → stop)
                    ▼
               assistant reply to user
                    │
                    ▼
             [session_save]
```

No box between `provider.chat` and `tools.exec`. No box between
`ToolResult` and `history.push`. That empty space is the design.

## Testing

- Unit tests for `history.rs` (append, trim-on-context-limit).
- Unit tests for `errors.rs` (tail-preserving truncation).
- Unit tests for `memory.rs` (`remember` appends, `recall` grep semantics).
- Integration test: mocked provider driving a 20-step task across the
  minimal core end-to-end.
- RSS regression test (see guardrail 4 above).
- Eval task suite (see rollout above). Acts as the promotion gate.

## Risks

- **Model ceiling.** If MiniMax M2 has a genuine blind spot on a task,
  the minimal loop cannot save it. It does, however, stop hiding the
  model's real behavior, which makes the ceiling visible.
- **Lost scaffolding that was load-bearing for a smaller model.** Some
  subsystems (e.g., `context_compressor`) may have been compensating
  for older/smaller models. Mitigation: feature flag, eval suite,
  no deletion until parity holds.
- **Tool-description effort.** Rewriting every tool description is
  unglamorous work with outsized impact. Budget it explicitly.
- **User-visible behavior drift.** Personality injector removal may
  change the agent's voice. Mitigation: one short paragraph of
  voice/tone guidance in the system prompt if desired — not a separate
  subsystem.

## Rollout Plan

1. Build `src/agent/core/` behind the `agent.core` flag. (~1 week)
2. Port `src/agent/eval.rs` into a task-suite runner and write the
   15–20 tasks. (~3 days)
3. Tool-description pass across `tool_descriptions/` and `tools/`.
   (~3 days)
4. Dogfood `--core=minimal` internally until the eval predicate holds.
5. Add the RSS regression test. Flip the default.
6. Delete the legacy `src/agent/` files and heavy `src/memory/` files
   listed above in a single PR, along with their tests and call-sites.

## Open Questions

- Should the markdown memory file be committed to the user's repo or
  stored outside it? (Default proposal: outside, under
  `$ZEROCLAW_HOME/memory/`.)
- Should `--core=minimal` be exposed to end users before promotion, or
  dogfooded only internally first? (Default proposal: internal-only
  until eval parity, then promote.)
- Exact RSS threshold for the 500-turn regression test — start at
  20 MB growth; tune during dogfooding.
