# WhatsApp Web Bulletproof Experience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make WhatsApp Web bulletproof and seamless — auto-reconnect, HTTP send API, presence keepalive, read receipts, typing, message dedup, real health, queue feedback.

**Architecture:** Three layers: Stability (reconnect, HTTP send, keepalive, webhook resilience), UX (receipts, typing, reply JID, coalescing), Hardening (dedup, health, queue). Each change is additive — no existing function signatures change. The WhatsApp flow protection contract applies.

**Tech Stack:** Rust, tokio, wa-rs, axum, SQLite (via existing RusqliteStore)

---

## ⚠️ CRITICAL: WhatsApp Flow Protection

- No function signature changes to `process_message()`, `run_tool_call_loop()`, `run_gateway_chat_with_tools()`
- All changes additive — existing consumers ignore new fields/variants
- Error recovery transparent
- The gateway `/whatsapp` webhook handler in `mod.rs` is NOT modified

---

## File Structure

| File | Responsibility | New/Modified |
|------|---------------|-------------|
| `src/channels/whatsapp_web.rs` | Auto-reconnect, keepalive, read receipts, typing, health_info | Modified |
| `src/channels/whatsapp_storage.rs` | Message dedup table and methods | Modified |
| `src/channels/mod.rs` | Webhook resilience, reply JID in payload, queue-with-typing | Modified |
| `src/gateway/api.rs` | HTTP send API endpoint | Modified |
| `src/gateway/mod.rs` | Wire whatsapp_web into AppState, health_info into /health | Modified |
| `src/config/schema.rs` | Message coalesce config fields | Modified |

---

### Task 1: Presence keepalive timer

**Files:**
- Modify: `src/channels/whatsapp_web.rs`

The auto-reconnect (StreamError → reconnect, liveness watchdog, last_event_at) is already implemented. This task adds the 4-minute presence keepalive.

- [ ] **Step 1: Add keepalive task after bot.run() in listen()**

In `src/channels/whatsapp_web.rs`, find the block after `bot.run().await?` and `*self.bot_handle.lock() = Some(bot_handle);` (around line 1619). Add after the `last_event_at` seeding block and before the `drop(logout_tx)` line:

```rust
            // Presence keepalive: send `available` every 4 minutes to prevent
            // WhatsApp servers from dropping the idle connection (~10 min timeout).
            let keepalive_client = bot.client();
            let keepalive_last_event = self.last_event_at.clone();
            let keepalive_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(
                    std::time::Duration::from_secs(240),
                );
                interval.set_missed_tick_behavior(
                    tokio::time::MissedTickBehavior::Skip,
                );
                loop {
                    interval.tick().await;
                    // Try presence().set_available(), fall back to chatstate if unavailable.
                    // This is a best-effort keepalive — failure is logged but not fatal.
                    match keepalive_client.send_presence_available().await {
                        Ok(()) => {
                            // Touch liveness timestamp so watchdog doesn't fire
                            keepalive_last_event.store(
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs(),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            tracing::debug!("WhatsApp Web: presence keepalive sent");
                        }
                        Err(e) => {
                            tracing::warn!("WhatsApp Web: presence keepalive failed: {e}");
                        }
                    }
                }
            });
```

**Note:** If `client.send_presence_available()` is not available on `wa_rs::Client`, check for `client.presence().set_available()` or `client.set_presence(PresenceType::Available)`. If none exist, use `client.send_raw_node(...)` with a presence stanza, or log a TODO and skip this step — the liveness watchdog will still catch dead connections.

- [ ] **Step 2: Abort keepalive task in cleanup block**

In the cleanup block (around line 1687, where `handle.abort()` is called for `bot_handle`), add before `drop(bot)`:

```rust
            keepalive_task.abort();
            let _ = keepalive_task.await;
```

The variable needs to be declared outside the `select!` scope. Move the `keepalive_task` declaration before the `select!` block, or restructure so it's accessible in cleanup. The simplest approach: declare `let keepalive_task: tokio::task::JoinHandle<()>;` before the block and assign it.

- [ ] **Step 3: Verify compilation**

Run: `cargo check --features whatsapp-web 2>&1 | tail -10`
Expected: Compiles. If `send_presence_available()` doesn't exist, check wa-rs docs and adapt.

- [ ] **Step 4: Commit**

```bash
git add src/channels/whatsapp_web.rs
git commit -m "feat(whatsapp-web): add 4-minute presence keepalive to prevent idle disconnect

Sends available presence every 240s through the live Baileys socket.
WhatsApp servers drop idle connections after ~10 minutes. Also touches
the liveness timestamp so the watchdog doesn't false-positive."
```

---

### Task 2: HTTP send API via daemon connection

**Files:**
- Modify: `src/gateway/mod.rs` (add whatsapp_web to AppState)
- Modify: `src/gateway/api.rs` (add POST /api/channels/whatsapp/send)

- [ ] **Step 1: Add whatsapp_web channel to AppState**

In `src/gateway/mod.rs`, add to the `AppState` struct (after the existing `whatsapp` field):

```rust
    pub whatsapp_web: Option<Arc<dyn Channel>>,
```

You'll need to add the import: `use crate::channels::traits::Channel;` (if not already imported).

In every place where `AppState` is constructed, add `whatsapp_web: None,` — search for `AppState {` in mod.rs and api.rs to find all construction sites. There are several (test helpers, gateway start, etc.).

Then, in the gateway startup function where channels are built (around where `channels_by_name` is constructed), capture the whatsapp channel and set it on AppState:

```rust
// After channels_by_name is built:
let whatsapp_web_channel = channels_by_name.get("whatsapp").cloned();
// Then when building AppState:
whatsapp_web: whatsapp_web_channel,
```

- [ ] **Step 2: Add the send endpoint in api.rs**

In `src/gateway/api.rs`, add the route handler. First, add the route to the router (find where routes are defined, add alongside existing `/api/*` routes):

```rust
.route("/api/channels/whatsapp/send", post(handle_whatsapp_send))
```

Then add the handler function:

```rust
#[derive(serde::Deserialize)]
struct WhatsAppSendRequest {
    recipient: String,
    message: String,
}

async fn handle_whatsapp_send(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<WhatsAppSendRequest>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let channel = match &state.whatsapp_web {
        Some(ch) => Arc::clone(ch),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "WhatsApp Web channel not connected",
                    "retryable": true,
                })),
            )
                .into_response();
        }
    };

    let send_msg = crate::channels::traits::SendMessage::new(
        body.message,
        &body.recipient,
    );

    match channel.send(&send_msg).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
            })),
        )
            .into_response(),
        Err(e) => {
            error_response(&e, "Failed to send WhatsApp message").into_response()
        }
    }
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check --features whatsapp-web 2>&1 | tail -10`
Expected: Compiles. Fix any missing imports or AppState construction sites.

- [ ] **Step 4: Commit**

```bash
git add src/gateway/api.rs src/gateway/mod.rs
git commit -m "feat(gateway): add POST /api/channels/whatsapp/send via daemon connection

Uses the daemon's live Baileys socket. Eliminates cold-send CLI which
opens a second concurrent connection and corrupts sequence numbers.
Returns 503 if channel not connected. Auth required."
```

---

### Task 3: Webhook timeout resilience

**Files:**
- Modify: `src/channels/mod.rs`

- [ ] **Step 1: Make webhook timeout non-fatal**

In `src/channels/mod.rs`, find the webhook forwarding block (around line 2499). Replace the error handling to distinguish timeout from other errors:

Find:
```rust
                    Err(e) => {
                        tracing::error!(
                            channel = "whatsapp",
                            sender = %msg.sender,
                            error = %e,
                            "Webhook forward failed"
                        );
                        None
                    }
```

Replace with:
```rust
                    Err(e) if e.is_timeout() => {
                        tracing::warn!(
                            channel = "whatsapp",
                            sender = %msg.sender,
                            "Webhook timed out after 30s — reply expected via HTTP send API"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::error!(
                            channel = "whatsapp",
                            sender = %msg.sender,
                            error = %e,
                            "Webhook forward failed"
                        );
                        None
                    }
```

The behavior is the same (None reply), but the log level is `warn` instead of `error` for timeouts, signaling this is expected when using the HTTP send API pattern.

- [ ] **Step 2: Verify compilation**

Run: `cargo check 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add src/channels/mod.rs
git commit -m "feat(whatsapp): treat webhook timeout as non-fatal warning

Distinguishes timeout from connection errors. Timeout is expected when
the admin backend uses the HTTP send API for async replies. Log level
changed from error to warn for timeout case."
```

---

### Task 4: Read receipt on message receipt

**Files:**
- Modify: `src/channels/whatsapp_web.rs`

- [ ] **Step 1: Send read receipt immediately in Event::Message handler**

In `src/channels/whatsapp_web.rs`, in the `Event::Message(msg, info)` handler, add immediately after the liveness timestamp update and before the sender_jid extraction (around line 1287):

```rust
                            Event::Message(msg, info) => {
                                // Send read receipt immediately — blue ticks signal the
                                // system is alive. Best-effort: failure is debug-logged.
                                {
                                    let receipt_chat = info.source.chat.clone();
                                    let receipt_sender = info.source.sender.clone();
                                    let receipt_id = info.id.clone();
                                    let receipt_client = client.clone();
                                    // Fire-and-forget: don't block message processing.
                                    tokio::spawn(async move {
                                        if let Err(e) = receipt_client
                                            .send_read_receipt(
                                                &receipt_chat,
                                                &receipt_sender,
                                                &[receipt_id],
                                            )
                                            .await
                                        {
                                            tracing::debug!(
                                                "WhatsApp Web: read receipt failed: {e}"
                                            );
                                        }
                                    });
                                }

                                let sender_jid = info.source.sender.clone();
```

**Note:** If `client.send_read_receipt(chat, sender, ids)` is not the correct wa-rs API, check for `client.read_receipt(info)` or `client.mark_read(chat, [id])`. Adapt the method name to match wa-rs.

- [ ] **Step 2: Verify compilation**

Run: `cargo check --features whatsapp-web 2>&1 | tail -10`

- [ ] **Step 3: Commit**

```bash
git add src/channels/whatsapp_web.rs
git commit -m "feat(whatsapp-web): send read receipt immediately on message receipt

Blue ticks appear instantly, signalling to the customer that the system
is alive. Fire-and-forget via tokio::spawn to avoid blocking message
processing. Failure is debug-logged (best-effort)."
```

---

### Task 5: Typing indicator until reply is sent

**Files:**
- Modify: `src/channels/whatsapp_web.rs`

- [ ] **Step 1: Start typing in Event::Message handler after read receipt**

Add immediately after the read receipt block in the Event::Message handler:

```rust
                                // Start typing immediately — customer sees "typing…"
                                // until the actual reply is sent via send().
                                {
                                    let typing_chat = info.source.chat.clone();
                                    let typing_client = client.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = typing_client
                                            .chatstate()
                                            .send_composing(&typing_chat)
                                            .await
                                        {
                                            tracing::debug!(
                                                "WhatsApp Web: start typing failed: {e}"
                                            );
                                        }
                                    });
                                }
```

- [ ] **Step 2: Stop typing before sending in send()**

In the `send()` method (around line 1060), add before the `client.send_message(to, outgoing).await?` call (line 1167):

```rust
        // Stop typing before sending — transition from "typing…" → message.
        let _ = client.chatstate().send_paused(&to).await;
```

- [ ] **Step 3: Add typing safety timeout**

Add a field to `WhatsAppWebChannel`:

```rust
    /// Chats currently showing typing indicator, with start time.
    /// Safety timeout clears typing after 120s to prevent phantom indicators.
    typing_started: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
```

Initialize in `new()`:
```rust
            typing_started: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
```

In the `Event::Message` handler typing block, record the start:
```rust
                                if let Ok(mut ts) = typing_started_clone.lock() {
                                    ts.insert(typing_chat.to_string(), std::time::Instant::now());
                                }
```

In `send()`, remove the entry when typing stops:
```rust
        if let Ok(mut ts) = self.typing_started.lock() {
            ts.remove(&message.recipient);
        }
```

Add a background task in `listen()` (alongside keepalive) that checks every 30s and sends `paused` for chats typing > 120s:

```rust
            let typing_cleanup_client = bot.client();
            let typing_cleanup_started = self.typing_started.clone();
            let typing_cleanup_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let stale: Vec<String> = typing_cleanup_started
                        .lock()
                        .map(|ts| {
                            ts.iter()
                                .filter(|(_, started)| started.elapsed().as_secs() > 120)
                                .map(|(chat, _)| chat.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    for chat in &stale {
                        if let Ok(jid) = chat.parse() {
                            let _ = typing_cleanup_client.chatstate().send_paused(&jid).await;
                        }
                    }
                    if !stale.is_empty() {
                        if let Ok(mut ts) = typing_cleanup_started.lock() {
                            for chat in &stale {
                                ts.remove(chat);
                            }
                        }
                    }
                }
            });
```

Abort this task in cleanup alongside keepalive.

- [ ] **Step 4: Verify compilation**

Run: `cargo check --features whatsapp-web 2>&1 | tail -10`

- [ ] **Step 5: Commit**

```bash
git add src/channels/whatsapp_web.rs
git commit -m "feat(whatsapp-web): keep typing indicator active until reply is sent

Typing starts on message receipt, stops in send() before message.
120s safety timeout clears phantom typing indicators. Decouples
typing from webhook response timing."
```

---

### Task 6: Canonical reply JID in webhook payload

**Files:**
- Modify: `src/channels/mod.rs`

- [ ] **Step 1: Add reply_to, message_id, and timestamp to webhook payload**

In `src/channels/mod.rs`, find the webhook payload construction (around line 2473):

Replace:
```rust
                let payload = serde_json::json!({
                    "sender": msg.sender,
                    "message": msg.content,
                    "channel": "whatsapp",
                });
```

With:
```rust
                let payload = serde_json::json!({
                    "sender": msg.sender,
                    "message": msg.content,
                    "channel": "whatsapp",
                    "reply_to": msg.reply_target,
                    "message_id": msg.id,
                    "timestamp": msg.timestamp,
                });
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add src/channels/mod.rs
git commit -m "feat(whatsapp): include reply_to JID, message_id, timestamp in webhook payload

Admin no longer needs to guess the correct JID format. reply_to
contains the canonical chat JID from wa-rs (e.g. 102688540897464@lid).
Use with POST /api/channels/whatsapp/send."
```

---

### Task 7: Message deduplication

**Files:**
- Modify: `src/channels/whatsapp_storage.rs`
- Modify: `src/channels/whatsapp_web.rs`

- [ ] **Step 1: Add dedup table and methods to RusqliteStore**

In `src/channels/whatsapp_storage.rs`, add to the `init_schema()` method (find the function that creates tables):

```rust
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS message_dedup (
                message_id TEXT PRIMARY KEY,
                processed_at INTEGER NOT NULL
            );"
        )?;
```

Add methods to `impl RusqliteStore`:

```rust
    /// Check if a message ID has already been processed (24h window).
    pub fn has_seen_message(&self, message_id: &str) -> bool {
        let conn = self.conn.lock();
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(86400); // 24h TTL
        conn.query_row(
            "SELECT 1 FROM message_dedup WHERE message_id = ?1 AND processed_at > ?2",
            rusqlite::params![message_id, cutoff as i64],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Mark a message ID as processed.
    pub fn mark_message_seen(&self, message_id: &str) {
        let conn = self.conn.lock();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO message_dedup (message_id, processed_at) VALUES (?1, ?2)",
            rusqlite::params![message_id, now as i64],
        );
    }

    /// Prune dedup entries older than 24 hours.
    pub fn prune_dedup(&self) {
        let conn = self.conn.lock();
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(86400);
        let _ = conn.execute(
            "DELETE FROM message_dedup WHERE processed_at < ?1",
            rusqlite::params![cutoff as i64],
        );
    }
```

- [ ] **Step 2: Add dedup store to WhatsAppWebChannel**

In `src/channels/whatsapp_web.rs`, add to the struct:

```rust
    /// Dedup store for preventing duplicate message processing across restarts.
    dedup_store: Option<Arc<RusqliteStore>>,
```

Initialize in `new()`:
```rust
            dedup_store: None,
```

In `listen()`, after creating the `RusqliteStore` backend, capture it for dedup:

```rust
            self.dedup_store = Some(Arc::clone(&backend));
```

- [ ] **Step 3: Check dedup in Event::Message handler**

In the `Event::Message` handler, after the liveness timestamp but before read receipt, add:

```rust
                                // Dedup: skip messages already processed (24h window).
                                let msg_id = info.id.clone();
                                if let Some(ref dedup) = dedup_store {
                                    if dedup.has_seen_message(&msg_id) {
                                        tracing::debug!(
                                            "WhatsApp Web: duplicate message {msg_id}, skipping"
                                        );
                                        return;
                                    }
                                    dedup.mark_message_seen(&msg_id);
                                }
```

The `dedup_store` needs to be cloned into the event handler closure. Add alongside the other clones:

```rust
            let dedup_store = self.dedup_store.clone();
```

And in the `async move` block, add it to captures.

- [ ] **Step 4: Add hourly prune task in listen()**

Alongside the keepalive task:

```rust
            let prune_store = self.dedup_store.clone();
            let prune_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    if let Some(ref store) = prune_store {
                        store.prune_dedup();
                        tracing::debug!("WhatsApp Web: pruned stale dedup entries");
                    }
                }
            });
```

Abort in cleanup.

- [ ] **Step 5: Verify compilation**

Run: `cargo check --features whatsapp-web 2>&1 | tail -10`

- [ ] **Step 6: Commit**

```bash
git add src/channels/whatsapp_storage.rs src/channels/whatsapp_web.rs
git commit -m "feat(whatsapp-web): add message deduplication with 24h TTL

Persists processed wa-rs message IDs to SQLite. Checks before
forwarding to webhook/processing. Prevents double replies across
restarts. Hourly prune of entries older than 24h."
```

---

### Task 8: Real socket state in /health

**Files:**
- Modify: `src/channels/whatsapp_web.rs`
- Modify: `src/gateway/mod.rs`

- [ ] **Step 1: Add health_info() method**

In `src/channels/whatsapp_web.rs`, add after `health_check()`:

```rust
    /// Structured health information for the /health endpoint.
    pub fn health_info(&self) -> serde_json::Value {
        let has_handle = self.bot_handle.lock().is_some();
        let has_client = self.client.lock().is_some();
        let last_event = self.last_event_at.load(std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        serde_json::json!({
            "ws_open": has_handle && has_client,
            "last_event_at": if last_event > 0 { Some(last_event) } else { None },
            "last_event_secs_ago": if last_event > 0 {
                Some(now.saturating_sub(last_event))
            } else {
                None
            },
            "healthy": has_handle && (last_event == 0 || now.saturating_sub(last_event) < 240),
        })
    }
```

- [ ] **Step 2: Wire into /health response**

In `src/gateway/mod.rs`, in the `handle_health` function (around line 1136), add the whatsapp_web health info to the response body:

```rust
        let whatsapp_web_health = state.whatsapp_web.as_ref().map(|ch| {
            // Downcast to WhatsAppWebChannel for health_info()
            if let Some(wa) = ch.as_any().downcast_ref::<crate::channels::whatsapp_web::WhatsAppWebChannel>() {
                wa.health_info()
            } else {
                serde_json::json!({"ws_open": false})
            }
        });
```

**Note:** This requires `as_any()` on the Channel trait. If it's not there, add to `src/channels/traits.rs`:

```rust
fn as_any(&self) -> &dyn std::any::Any { unimplemented!() }
```

With a default implementation, then implement it for WhatsAppWebChannel:

```rust
fn as_any(&self) -> &dyn std::any::Any { self }
```

Alternatively, store the `WhatsAppWebChannel` as a typed `Option<Arc<WhatsAppWebChannel>>` in AppState instead of `Option<Arc<dyn Channel>>` to avoid downcasting.

Include in the health body:
```rust
        let body = serde_json::json!({
            "status": "ok",
            "paired": state.pairing.is_paired(),
            "require_pairing": state.pairing.require_pairing(),
            "runtime": crate::health::snapshot_json(),
            "whatsapp_web": whatsapp_web_health,
        });
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check --features whatsapp-web 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add src/channels/whatsapp_web.rs src/gateway/mod.rs
git commit -m "feat(whatsapp-web): add real socket state to /health endpoint

Reports ws_open (actual client connection), last_event_at, and
last_event_secs_ago. Replaces the previous bot_handle.is_some()
check that always reported healthy even when the stream was dead."
```

---

### Task 9: Queue messages when at in-flight limit

**Files:**
- Modify: `src/channels/mod.rs`

- [ ] **Step 1: Replace blocking semaphore acquire with try-then-typing pattern**

In `src/channels/mod.rs`, find the main semaphore acquire (around line 3797):

Replace:
```rust
        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
```

With:
```rust
        let permit = match Arc::clone(&semaphore).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // At capacity — show typing so customer knows we're alive.
                if let Some(ch) = ctx.channels_by_name.get(&msg.channel).or_else(|| {
                    msg.channel
                        .split_once(':')
                        .and_then(|(base, _)| ctx.channels_by_name.get(base))
                }) {
                    let _ = ch.start_typing(&msg.reply_target).await;
                }
                tracing::info!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "Message queued — at in-flight limit, waiting for capacity"
                );
                match Arc::clone(&semaphore).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                }
            }
        };
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add src/channels/mod.rs
git commit -m "feat(channels): show typing indicator when message is queued at capacity

Uses try_acquire_owned first. If at limit, starts typing to signal
the system is alive, then blocks on acquire. Customer sees typing
instead of silence while waiting for a processing slot."
```

---

### Task 10: Message coalesce config

**Files:**
- Modify: `src/config/schema.rs`

- [ ] **Step 1: Add coalesce fields to WhatsAppConfig**

In `src/config/schema.rs`, find the `WhatsAppConfig` struct and add after the existing fields:

```rust
    /// Initial coalesce window (ms) for multi-message bursts. Default: 300.
    #[serde(default = "default_wa_coalesce_ms")]
    pub message_coalesce_ms: u64,
    /// Extension per additional message during coalesce window (ms). Default: 1000.
    #[serde(default = "default_wa_coalesce_extend_ms")]
    pub message_coalesce_extend_ms: u64,
    /// Maximum coalesce window (ms). Default: 5000.
    #[serde(default = "default_wa_coalesce_max_ms")]
    pub message_coalesce_max_ms: u64,
```

Add the default functions:
```rust
fn default_wa_coalesce_ms() -> u64 { 300 }
fn default_wa_coalesce_extend_ms() -> u64 { 1000 }
fn default_wa_coalesce_max_ms() -> u64 { 5000 }
```

- [ ] **Step 2: Wire coalesce config into the debouncer**

The existing debouncer in `mod.rs` already handles message coalescing. This task just exposes the WhatsApp-specific timing via config. The debouncer's window parameters should read from these config fields when the channel is "whatsapp". This wiring depends on how the debouncer is currently configured — check the debouncer initialization and adapt.

If the debouncer doesn't support per-channel profiles yet, create a TODO comment noting this as a future enhancement and use the global debounce settings.

- [ ] **Step 3: Verify compilation**

Run: `cargo check 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add src/config/schema.rs
git commit -m "feat(config): add WhatsApp message coalesce timing config

Configurable initial window (300ms), extension (1000ms), and max
window (5000ms) for catching rapid multi-message bursts. Uses
existing debouncer infrastructure."
```

---

### Task 11: Full validation

- [ ] **Step 1: Run cargo fmt**

Run: `cargo fmt --all`

- [ ] **Step 2: Run cargo clippy**

Run: `cargo clippy --all-targets --features whatsapp-web -- -D warnings 2>&1 | tail -20`

- [ ] **Step 3: Run tests**

Run: `cargo test --lib 2>&1 | tail -30`

- [ ] **Step 4: Check whatsapp-specific tests**

Run: `cargo test --features whatsapp-web --lib whatsapp 2>&1 | tail -20`

- [ ] **Step 5: Commit any fixes**

```bash
git add -A
git commit -m "chore: fix clippy warnings and rustfmt from whatsapp bulletproof changes"
```
