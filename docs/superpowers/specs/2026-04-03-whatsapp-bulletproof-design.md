# WhatsApp Web Bulletproof Experience Design

**Date:** 2026-04-03
**Status:** Draft
**Risk tier:** High (touches WhatsApp channel, gateway, message pipeline)

## Goal

Make the WhatsApp Web channel bulletproof and seamless for customers. Eliminate silent failures, remove cold-send race conditions, add real-time feedback (read receipts, typing), and harden message delivery with dedup and queuing.

## Current Problems

1. **Silent stream death:** `Event::StreamError` is logged but doesn't trigger reconnect. The bot handle stays alive while the WebSocket is dead. No keepalive detects the dropped connection.
2. **Cold send corrupts session:** `cold_send()` opens a second Baileys connection to the same session DB, corrupting sequence numbers and killing the receive stream.
3. **No presence keepalive:** WhatsApp servers drop idle connections after ~10 minutes. No heartbeat is sent.
4. **Webhook timeout drops messages:** If the admin webhook exceeds the 30s timeout, the message is lost — no retry, no queue.
5. **No read receipts:** Customer sees single grey tick instead of blue ticks.
6. **Typing depends on webhook:** Typing indicator stops when webhook returns, not when reply is sent.
7. **No reply JID in webhook:** Admin must guess the correct JID format.
8. **No message coalescing:** Rapid multi-message bursts each trigger separate LLM calls.
9. **No deduplication:** WhatsApp retransmits cause duplicate processing.
10. **Health check is fake:** Only checks `bot_handle.is_some()`, not actual socket state.
11. **Messages dropped at limit:** When semaphore is full, new messages are silently queued on the mpsc channel with no user feedback.

---

## 1. Auto-Reconnect on Stream Drop

**Pattern:** Listen for `Event::StreamError` and liveness watchdog. Reconnect the Baileys socket in-place with exponential backoff. No daemon restart.

### Changes to `src/channels/whatsapp_web.rs`

**Already implemented (partial fix):**
- `Event::StreamError` now sends on `logout_tx` to trigger reconnect
- Liveness watchdog: polls `last_event_at` every 30s, triggers reconnect if no events in 120s
- `last_event_at` (`AtomicU64`) stamped on every event in the callback

**Additional:** The `session_revoked` flag must NOT be set for `StreamError` events — the session is still valid, only the socket died. The existing logic already handles this correctly: `session_revoked` is only set on `Event::LoggedOut`, so a `StreamError`-triggered reconnect will reuse the existing session.

### Reconnect behavior

- `StreamError` → `logout_tx.send(())` → `select!` resolves → cleanup → retry with existing session
- Liveness timeout (120s no events) → same path
- Exponential backoff: 3s, 6s, 12s, 24s, 48s, 96s, 192s, 300s (capped)
- Max 10 retries before giving up (existing `MAX_RETRIES`)
- Retry counter resets on `Event::Connected`

---

## 2. HTTP Send API via Daemon Connection

**Pattern:** `POST /api/channels/whatsapp/send` using the daemon's live Baileys socket. Eliminates cold send entirely for programmatic sends.

### New route in `src/gateway/api.rs`

```
POST /api/channels/whatsapp/send
Content-Type: application/json
Authorization: Bearer <token>

{
  "recipient": "15551234567" | "15551234567@s.whatsapp.net" | "102688540897464@lid",
  "message": "Hello from the API"
}

Response 200:
{
  "success": true,
  "message_id": "3EB0..."
}

Response 503:
{
  "error": "WhatsApp Web channel not connected",
  "retryable": true
}
```

### Implementation

- Handler looks up `"whatsapp"` in `AppState.channels_by_name`
- Calls `channel.send(&SendMessage::new(message, recipient))`
- Uses the live daemon connection — no cold send, no second socket
- Returns the wa-rs message ID for tracking
- 503 if channel not connected (client is None)

### Cold send deprecation

`cold_send()` remains as fallback for CLI `zeroclaw channel send` when no daemon is running, but is never used when the daemon is active. Add a log warning when cold_send is invoked while a daemon is running.

---

## 3. Presence Keepalive Every 4 Minutes

**Pattern:** Background timer sends `available` presence through the live socket to prevent WhatsApp server idle timeout (~10 min).

### Implementation in `listen()`

After `bot.run().await?`, spawn a keepalive task:

```rust
let keepalive_client = client.clone();
let keepalive_task = tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(240)); // 4 minutes
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(e) = keepalive_client.presence().set_available().await {
            tracing::warn!("WhatsApp Web: presence keepalive failed: {e}");
        } else {
            tracing::debug!("WhatsApp Web: presence keepalive sent");
        }
    }
});
```

Abort `keepalive_task` in the cleanup block alongside `bot_handle.abort()`.

### Fallback

If `presence().set_available()` is not available on the wa-rs `Client`, use `chatstate()` or a lightweight no-op message. Check the wa-rs API at implementation time.

---

## 4. Webhook Timeout Resilience

**Pattern:** If the admin webhook exceeds the 30s timeout, treat it as non-fatal. The message was already forwarded — the reply will arrive independently via the HTTP send API.

### Changes to `src/channels/mod.rs` webhook forwarding (line 2499)

Current behavior: timeout error → `None` reply → message effectively dropped.

New behavior:
- On timeout: log warning, stop typing, continue (don't `return` early)
- The admin backend is expected to send the reply via `POST /api/channels/whatsapp/send` when ready
- On non-timeout errors (DNS, connection refused): same as current — log and continue

```rust
Err(e) if e.is_timeout() => {
    tracing::warn!(
        channel = "whatsapp",
        sender = %msg.sender,
        "Webhook timed out after 30s — reply expected via HTTP send API"
    );
    None
}
```

The key change: after the webhook block, do NOT `return` when webhook_reply is None and the webhook was configured. Instead, let the message be marked as "forwarded, awaiting async reply."

**Note:** The typing indicator should keep running (see UX section) until the reply arrives via the send API. This is handled by decoupling typing from webhook response.

---

## 5. Read Receipt on Message Receipt

**Pattern:** Call wa-rs read receipt before forwarding to webhook/processing. Blue ticks appear instantly.

### Implementation in the `Event::Message` handler

After extracting message content and before `tx_inner.send(...)`:

```rust
// Send read receipt immediately — blue ticks signal the system is alive.
if let Err(e) = client.read_receipt(&info).await {
    tracing::debug!("WhatsApp Web: failed to send read receipt: {e}");
}
```

If `client.read_receipt(&info)` is not available on wa-rs, use the message key directly:
```rust
client.send_read_receipt(&info.source.chat, &[info.id.clone()]).await
```

The exact API depends on wa-rs — check at implementation time. This is best-effort (failure is debug-logged, not error).

---

## 6. Typing Indicator Until Send API Reply

**Pattern:** Show typing from message receipt, stop only when the reply is actually sent via `send()`. Remove dependency on webhook response timing.

### Implementation

**Current:** Typing starts when webhook request begins, stops when webhook response arrives.

**New:**
1. In the `Event::Message` handler, immediately after read receipt, start typing:
   ```rust
   if let Err(e) = client.chatstate().send_composing(&info.source.chat).await {
       tracing::debug!("WhatsApp Web: failed to start typing: {e}");
   }
   ```

2. In `send()`, stop typing before sending the actual message:
   ```rust
   // Stop typing before sending — transition from "typing…" → message.
   let _ = client.chatstate().send_paused(&to).await;
   ```

3. The webhook forward block in `mod.rs` no longer manages typing — it's managed by the channel itself.

4. Add a safety timeout: if no `send()` happens within 120s of starting typing, auto-stop to avoid phantom typing indicators.

### Typing safety timeout

Track `typing_started_at: Arc<Mutex<HashMap<String, Instant>>>` on the channel struct. A background task checks every 30s and sends `paused` for any chat that's been typing > 120s.

---

## 7. Canonical Reply JID in Webhook Payload

**Pattern:** Include `"reply_to"` in the forwarded webhook body so the admin doesn't need to guess.

### Changes to `src/channels/mod.rs` webhook payload (line 2473)

Add `reply_to` field:

```rust
let payload = serde_json::json!({
    "sender": msg.sender,
    "message": msg.content,
    "channel": "whatsapp",
    "reply_to": msg.reply_target,  // e.g. "102688540897464@lid"
    "message_id": msg.id,
    "timestamp": msg.timestamp,
});
```

The `reply_target` already contains the correct chat JID from the `Event::Message` handler (set at line 1509). This is the canonical JID that `send()` accepts.

---

## 8. Presence-Aware Message Coalescing

**Pattern:** On message receipt, wait briefly for multi-message bursts. Subscribe to sender presence to detect "still typing." Flush on pause/timeout.

### Design

The existing debouncer in `mod.rs` already coalesces messages with a configurable window. Enhance it for WhatsApp specifically:

1. On first message from a sender, start a 300ms timer
2. Subscribe to sender's presence (if wa-rs supports it)
3. If `composing` event fires within the window → extend by 1s
4. If `paused` fires or timer expires → flush immediately
5. Combined messages separated by `\n`

### Implementation

Since wa-rs presence subscription may not be available, use a simpler approach:

- Use the existing debouncer with a WhatsApp-specific profile:
  - `initial_window_ms: 300`
  - `extension_on_new_message_ms: 1000`
  - `max_window_ms: 5000`
- This catches rapid multi-message bursts without presence subscription
- Presence subscription can be added later as an enhancement

### Config addition

```toml
[channels_config.whatsapp]
message_coalesce_ms = 300       # Initial wait window
message_coalesce_extend_ms = 1000  # Extension per additional message
message_coalesce_max_ms = 5000  # Maximum coalesce window
```

---

## 9. Message Deduplication

**Pattern:** Persist processed wa-rs message IDs to SQLite with 24h TTL. Check before forwarding.

### Implementation

Add a `dedup` table to the existing session SQLite database:

```sql
CREATE TABLE IF NOT EXISTS message_dedup (
    message_id TEXT PRIMARY KEY,
    processed_at INTEGER NOT NULL  -- unix epoch seconds
);
```

In the `Event::Message` handler, before processing:

```rust
let msg_id = info.id.clone();
if dedup_store.has_seen(&msg_id).await {
    tracing::debug!("WhatsApp Web: duplicate message {msg_id}, skipping");
    return;
}
dedup_store.mark_seen(&msg_id).await;
```

### Cleanup

Prune entries older than 24h on startup and every hour:
```sql
DELETE FROM message_dedup WHERE processed_at < ?
```

### Storage

Reuse the existing `RusqliteStore` connection via an additional method, or create a lightweight `DedupStore` wrapper around the same SQLite path.

---

## 10. Real Socket State in /health

**Pattern:** Report actual WebSocket state and last message timestamp.

### Changes to `health_check()` (already partially implemented)

The current implementation already checks `last_event_at` against a 240s window. Extend to return structured health info.

Add a new method:

```rust
pub fn health_info(&self) -> serde_json::Value {
    let has_handle = self.bot_handle.lock().is_some();
    let has_client = self.client.lock().is_some();
    let last_event = self.last_event_at.load(Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    serde_json::json!({
        "ws_open": has_handle && has_client,
        "last_event_at": if last_event > 0 { Some(last_event) } else { None },
        "last_event_secs_ago": if last_event > 0 { Some(now.saturating_sub(last_event)) } else { None },
        "healthy": has_handle && (last_event == 0 || now.saturating_sub(last_event) < 240),
    })
}
```

Wire into the gateway `/health` response under the channel's component entry.

---

## 11. Queue Messages When at In-Flight Limit

**Pattern:** When the semaphore is full, queue instead of silently waiting. Show typing until processing starts.

### Current behavior

The semaphore `.acquire_owned().await` blocks the dispatch loop — incoming messages pile up in the mpsc channel buffer. No feedback to sender.

### New behavior

1. Use `semaphore.try_acquire_owned()` first
2. If acquired → dispatch immediately
3. If full → start typing to the sender, then `semaphore.acquire_owned().await`
4. When permit acquired → dispatch worker (typing continues until reply sent)

```rust
let permit = match Arc::clone(&semaphore).try_acquire_owned() {
    Ok(permit) => permit,
    Err(_) => {
        // At capacity — show typing so customer knows we're alive.
        if let Some(ch) = ctx.channels_by_name.get(&msg.channel) {
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

---

## Files Changed

| File | Change type | Risk |
|------|------------|------|
| `src/channels/whatsapp_web.rs` | Stream reconnect, keepalive, read receipts, typing, dedup, health | High |
| `src/channels/whatsapp_storage.rs` | Add dedup table/methods | Medium |
| `src/channels/mod.rs` | Webhook resilience, reply JID, queue typing, coalesce config | High |
| `src/gateway/api.rs` | HTTP send API endpoint | Medium |
| `src/gateway/mod.rs` | Wire health_info into /health | Low |
| `src/config/schema.rs` | Message coalesce config fields | Low |

## Dependencies

No new crates needed. All features use existing wa-rs, tokio, serde_json, reqwest.

## Testing Strategy

- Auto-reconnect: integration test with mock event stream that emits StreamError
- HTTP send API: unit test verifying route handler returns correct responses
- Presence keepalive: verify timer spawns and calls presence API
- Webhook timeout: unit test with mock webhook that sleeps > 30s
- Read receipt: verify receipt is sent before message forwarding
- Dedup: unit test with duplicate message IDs
- Health: unit test verifying structured health info
- Queue feedback: unit test verifying typing is started when semaphore is full

## Out of Scope

- Presence subscription for coalescing (future enhancement — use timer-based for now)
- Media message dedup (voice notes, images — only text dedup for now)
- Multi-device session coordination (wa-rs handles this internally)
