# ZeroClaw SQLite Blocking Fix Plan

## Problem Summary

The `/api/chat` endpoint experiences 15+ second delays because:
1. Memory operations (`recall()` and `store()`) block on SQLite mutex contention
2. Cron scheduler runs 2 agent jobs that hold SQLite locks for long periods
3. Memory hygiene tasks run concurrently with API requests

**Current flow (broken):**
```
Request → memory.recall() → BLOCKED 10s → memory.store() → BLOCKED 5s → LLM call 7s → Response
Total: ~22 seconds (15s wasted on timeouts)
```

**Target flow:**
```
Request → skip memory ops → LLM call 3-7s → Response
Total: 3-7 seconds
```

---

## Fix Options (Choose One)

### Option A: Disable Memory for /api/chat (Recommended - Fastest)

The `/api/chat` endpoint is used for simple request/response - it doesn't need memory context or auto-save.

**Files to modify:**
- `/home/ubuntu/zeroclaw/src/gateway/api.rs`

**Steps:**

1. **Open the file:**
   ```bash
   cd /home/ubuntu/zeroclaw
   code src/gateway/api.rs  # or vim/nano
   ```

2. **Find the `handle_http_chat` function** (around line 1631):
   ```rust
   pub(super) async fn handle_http_chat(
   ```

3. **After getting the agent, disable auto_save and use NoneMemory:**

   Find this code block (around line 1640-1645):
   ```rust
   let mut agent = if let Some(ref cached) = state.cached_agent {
       tracing::info!("Using pre-warmed agent for /api/chat");
       let guard = cached.lock().await;
       let mut cloned = guard.clone();
       cloned.reset_for_request();
       cloned
   ```

   **Change to:**
   ```rust
   let mut agent = if let Some(ref cached) = state.cached_agent {
       tracing::info!("Using pre-warmed agent for /api/chat");
       let guard = cached.lock().await;
       let mut cloned = guard.clone();
       cloned.reset_for_request();
       // Disable memory operations for /api/chat - they cause blocking
       cloned.set_auto_save(false);
       cloned
   ```

4. **Also disable memory loading in the Agent itself.**

   Open `/home/ubuntu/zeroclaw/src/agent/agent.rs` and find the `turn()` function.

   Find this section (around line 782):
   ```rust
   tracing::debug!("[turn] Loading memory context");
   let context_future = self.memory_loader.load_context(
   ```

   **Add a skip condition before memory loading:**
   ```rust
   // Skip memory loading if auto_save is disabled (e.g., /api/chat endpoint)
   let context = if !self.auto_save {
       tracing::debug!("[turn] Skipping memory context (auto_save disabled)");
       String::new()
   } else {
       tracing::debug!("[turn] Loading memory context");
       let context_future = self.memory_loader.load_context(
           self.memory.as_ref(),
           user_message,
           self.memory_session_id.as_deref(),
       );
       match tokio::time::timeout(
           std::time::Duration::from_secs(10),
           context_future,
       )
       .await
       {
           Ok(Ok(ctx)) => {
               tracing::debug!("[turn] Memory context loaded, len={}", ctx.len());
               ctx
           }
           Ok(Err(e)) => {
               tracing::warn!("[turn] Memory context load failed: {e}");
               String::new()
           }
           Err(_) => {
               tracing::warn!("[turn] Memory context load timed out after 10s, proceeding without context");
               String::new()
           }
       }
   };
   ```

5. **Build and test:**
   ```bash
   cd /home/ubuntu/zeroclaw
   docker build -t zeroclaw:test .

   # Test locally first
   docker run --rm -it --network naseyma_default \
     -v /home/ubuntu/zeroclaw-data:/zeroclaw-data:rw \
     -p 42618:42617 \
     zeroclaw:test daemon &

   # Wait 10s for startup, then test
   sleep 10
   time curl -s -X POST "http://localhost:42618/api/chat" \
     -H "Content-Type: application/json" \
     -d '{"message":"Say hi"}'

   # Should complete in 3-7 seconds, not 20+
   ```

6. **Deploy to production:**
   ```bash
   docker tag zeroclaw:test ghcr.io/samelamin/zeroclaw:whatsapp-claude
   cd /home/ubuntu/naseyma
   docker compose up -d zeroclaw
   ```

---

### Option B: Disable Cron Scheduler Entirely

If the cron jobs aren't needed (zeroclaw is just a WhatsApp router), disable them.

**Files to modify:**
- Zeroclaw config (inside container or baked into image)

**Steps:**

1. **Check if cron can be disabled via environment:**
   ```bash
   # Add to docker-compose.yml under zeroclaw service:
   environment:
     - ZEROCLAW_CRON_ENABLED=false
   ```

2. **Or modify the config.toml baked into the image:**

   Edit `/home/ubuntu/zeroclaw/config/config.toml` (or wherever the default config is):
   ```toml
   [cron]
   enabled = false
   catch_up_on_startup = false
   ```

3. **Rebuild and deploy:**
   ```bash
   cd /home/ubuntu/zeroclaw
   docker build -t ghcr.io/samelamin/zeroclaw:whatsapp-claude .
   cd /home/ubuntu/naseyma
   docker compose up -d zeroclaw
   ```

4. **Verify cron is disabled:**
   ```bash
   docker logs zeroclaw 2>&1 | grep -i cron
   # Should NOT show "catching up overdue jobs"
   ```

---

### Option C: Add SQLite busy_timeout (Reduces but doesn't eliminate delay)

This makes SQLite retry instead of immediately blocking, but concurrent writes still wait.

**Files to modify:**
- `/home/ubuntu/zeroclaw/src/memory/sqlite.rs`

**Steps:**

1. **Find the PRAGMA initialization** (around line 60):
   ```rust
   conn.execute_batch(
       "PRAGMA journal_mode = WAL;
        PRAGMA synchronous  = NORMAL;
        PRAGMA mmap_size    = 8388608;
        PRAGMA cache_size   = -2000;
        PRAGMA temp_store   = MEMORY;",
   )?;
   ```

2. **Add busy_timeout:**
   ```rust
   conn.execute_batch(
       "PRAGMA journal_mode = WAL;
        PRAGMA synchronous  = NORMAL;
        PRAGMA mmap_size    = 8388608;
        PRAGMA cache_size   = -2000;
        PRAGMA temp_store   = MEMORY;
        PRAGMA busy_timeout = 5000;",  // Wait up to 5 seconds for locks
   )?;
   ```

3. **Also add it to the other PRAGMA block** (around line 107) - search for all `PRAGMA journal_mode` occurrences.

4. **Build and deploy.**

---

## Recommended Approach

**Do Option A first** - it's the cleanest solution and makes `/api/chat` independent of memory operations entirely.

If customers need memory context in `/api/chat` responses, then:
1. Do Option B (disable cron) to eliminate the blocking source
2. Or do Option C as a fallback to reduce lock wait times

---

## Verification Checklist

After deploying, verify:

```bash
# 1. Check startup logs - no cron catching up
docker logs zeroclaw 2>&1 | head -50

# 2. Test response time
time curl -s -X POST "http://localhost:42617/api/chat" \
  -H "Content-Type: application/json" \
  -d '{"message":"Hello"}'
# Expected: 3-7 seconds

# 3. Check debug logs show skip
docker logs zeroclaw 2>&1 | grep -E "\[turn\]" | tail -10
# Should show: "[turn] Skipping memory context (auto_save disabled)"

# 4. Run multiple concurrent requests
for i in {1..5}; do
  time curl -s -X POST "http://localhost:42617/api/chat" \
    -H "Content-Type: application/json" \
    -d '{"message":"Test '$i'"}' &
done
wait
# All should complete in ~7s, not sequentially blocked
```

---

## Code Locations Reference

| File | Line | Purpose |
|------|------|---------|
| `src/gateway/api.rs` | ~1631 | `handle_http_chat` function |
| `src/gateway/api.rs` | ~1640 | Agent clone and reset |
| `src/agent/agent.rs` | ~766 | `turn()` function start |
| `src/agent/agent.rs` | ~782 | Memory context loading |
| `src/agent/agent.rs` | ~794 | Auto-save logic |
| `src/memory/sqlite.rs` | ~60 | SQLite PRAGMA init |
| `src/cron/scheduler.rs` | ~254 | `run_agent_job` (creates new agents) |

---

## Contact

If you get stuck:
1. Check the logs: `docker logs zeroclaw 2>&1 | tail -100`
2. Look for `[turn]` debug messages to see where it's blocking
3. The timeout fallback (already deployed) ensures requests never hang forever
