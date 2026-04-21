#!/usr/bin/env bash
#
# Fails CI if banned growth-prone patterns appear in src/agent/ or src/memory/.
# These are the five patterns identified in the agent-recore spec as the root
# cause of the brain.db memory snowball. New code must avoid them; pre-existing
# hits in load-bearing files are allowlisted below.

set -euo pipefail

# -----------------------------------------------------------------------------
# Patterns we refuse to accept in new code (POSIX-regex for grep -E).
# -----------------------------------------------------------------------------
PATTERNS=(
  'LazyLock<Mutex<HashMap'
  'LazyLock<Mutex<Vec'
  'OnceLock<Mutex<'
  'static mut '
  'tokio::spawn\('
)

# -----------------------------------------------------------------------------
# Files allowlisted because they contain production-necessary patterns.
# Remove entries here only when the pattern is genuinely eliminated.
# -----------------------------------------------------------------------------
ALLOWLIST=(
  # --- Phase-7 remaining files (still have external callers, not yet deleted) ---
  'src/agent/loop_.rs'
  'src/agent/microcompactor.rs'
  'src/agent/loop_detector.rs'
  'src/agent/history_pruner.rs'
  'src/agent/context_compressor.rs'
  'src/agent/classifier.rs'
  'src/agent/personality.rs'
  'src/agent/thinking.rs'
  'src/memory/decay.rs'
  'src/memory/knowledge_graph.rs'
  'src/memory/procedural.rs'
  'src/memory/embeddings.rs'
  'src/memory/vector.rs'
  'src/memory/qdrant.rs'
  'src/memory/chunker.rs'

  # --- Pre-existing load-bearing files (NOT scheduled for deletion) ---
  # These hold production tokio::spawn() calls that are:
  #   - agent.rs: the tool-listener and webhook server (fire once, long-lived
  #     on purpose; lifetime is bound to the Agent itself).
  #   - sqlite.rs: bulk-write parallelism; spawned handles are collected into
  #     a Vec and awaited before the function returns — scoped, not leaking.
  #   - battle_tests.rs: test harness; scoped to one test invocation.
  # New scaffolding code must still avoid tokio::spawn; do NOT extend this
  # section without a review comment explaining why.
  'src/agent/agent.rs'
  'src/memory/sqlite.rs'
  'src/memory/battle_tests.rs'
)

FAIL=0
for pattern in "${PATTERNS[@]}"; do
  # Build a --exclude arg per allowlisted file.
  EXCLUDES=()
  for path in "${ALLOWLIST[@]}"; do
    EXCLUDES+=(--exclude="$(basename "$path")")
  done

  # Search src/agent/ and src/memory/ (recursive), excluding allowlisted files.
  HITS=$(grep -rnE "$pattern" src/agent/ src/memory/ "${EXCLUDES[@]}" || true)
  if [[ -n "$HITS" ]]; then
    echo "::error::Forbidden pattern '$pattern' found in src/agent/ or src/memory/:"
    echo "$HITS"
    FAIL=1
  fi
done

# Also: any tokio::spawn() in src/agent/core-adjacent files (not allowlisted)
# that spawns a long-lived loop is banned. The grep above already catches the
# bare call; rely on code review for the "long-lived" nuance.

if [[ $FAIL -ne 0 ]]; then
  echo ""
  echo "Agent-core guardrail violated. See spec:"
  echo "  docs/superpowers/specs/2026-04-21-agent-recore-design.md"
  echo "  §Memory-leak guardrails (binding rules for the new core)"
  exit 1
fi

echo "agent-core guardrail: OK"
