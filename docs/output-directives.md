# Output Directives

## Overview

Agents can control platform-specific message delivery by prefixing their output with `[[key:value]]` directives. OAB parses and strips these before sending to the platform.

## Format

```
[[reply_to:1502606076451885136]]
[[ephemeral:true]]              ← future
Actual message content starts here...
```

Rules:
- Consecutive `[[key:value]]` lines at the start of output = directive header block
- First line that doesn't match `[[key:value]]` (with colon) = content begins
- `[[X]]` without colon is NOT a directive — stops parsing, preserved as content
- Directives are stripped from the final message (never visible to users)
- Unknown keys are silently ignored (forward compatible, logged at debug level)
- If the same key appears multiple times, the last value wins

## Available Directives

### `reply_to`

Reply to a specific message by ID (Discord: `message_reference`).

```
[[reply_to:1502606076451885136]]
Here is my reply to that specific message.
```

**Value**: Platform message ID. Format depends on the target adapter — Discord requires a numeric snowflake; Slack accepts `ts` (e.g. `1234567890.123456`). The directive parser validates that the value is non-empty, ≤64 chars, and contains only ASCII alphanumeric characters plus `.`, `-`, `_`; per-platform format validation happens in each adapter.

**Behavior**:
- Discord: sends with `message_reference`, showing the native "replying to..." UI
- Feishu: sends via Reply API (`POST /im/v1/messages/{id}/reply`), showing native quote UI
- Invalid/non-existent message ID: silently falls back to plain send
- Works in both streaming and send-once modes

**How agents get message IDs**: Every incoming message includes `message_id` in `SenderContext`:

```json
{
  "schema": "openab.sender.v1",
  "sender_id": "845835116920307722",
  "sender_name": "pahud.hsieh",
  "message_id": "1502606076451885136",
  "channel": "discord",
  ...
}
```

## Multi-Agent Use Case

In a thread with multiple bots, agents can reply to each other's messages:

```
Human: "Review this PR" (message_id: 100)
Bot A: "Found 3 issues" (message_id: 101)
Bot B output:
  [[reply_to:101]]
  I agree with Bot A on F1, but F2 is actually fine because...
```

This creates clear visual conversation threads within a Discord thread — essential for multi-agent collaboration.

### `attach`

Attach a local file to the reply — uploaded to the platform as a real file attachment.

```
[[attach:report.pdf]]
[[attach:charts/usage.png]]
Here is the report you asked for.
```

**Value**: A path to a file on disk. Relative paths resolve against the session's working directory; `~` expands to the bot home; absolute paths are accepted as long as they stay in scope.

**Scope**: Files must live inside the session workspace or the bot home. Paths are canonicalized before checking, so `../` escapes and symlinks pointing outside are rejected — a rejected directive produces a `⚠️ Couldn't attach …` note in the thread.

**Limits**: ≤25 MiB per file, ≤10 attachments per turn.

**Behavior**:
- Discord: uploaded as a message attachment (serenity `CreateAttachment`)
- Slack: uploaded via `files.uploadV2` (`files.getUploadURLExternal` → `completeUploadExternal`) into the channel/thread — requires the **`files:write`** OAuth scope
- Other platforms: falls back to a `⚠️ Couldn't attach …` text notice

### Markdown images

Local-path markdown images are attached implicitly — no directive needed:

```
Here's the chart:
![usage chart](charts/usage.png)
```

If `charts/usage.png` resolves inside the workspace, the markup is stripped and the file is uploaded. Remote URLs (`https://…`, `data:…`), missing files, and out-of-scope paths are left as-is (out-of-scope and oversized files also produce a warning note).

> Tip for agents: write the file to the workspace first, then reference it — either mechanism works, `[[attach:]]` is the explicit/documented form.

## Comparison with Other Platforms

| Platform | Reply Mechanism | Agent Control |
|----------|----------------|---------------|
| OpenClaw | `replyToMode` config (`off`/`first`/`all`) | ❌ Platform decides, always to trigger msg |
| Hermes Agent | `DISCORD_REPLY_TO_MODE` env var | ❌ Platform decides, always to trigger msg |
| **OAB** | `[[reply_to:message_id]]` directive | ✅ Agent chooses any message |

> **Note:** `reply_to` is currently implemented for Discord and Feishu (gateway). Slack behavior depends on `assistant_mode`:
>
> - **`assistant_mode = true` (default):** When native streaming is active, the `reply_to` directive is **bypassed** — the streamed message is itself the in-thread reply and cannot target a different message. The directive is silently ignored (no error).
> - **`assistant_mode = false`:** The Slack adapter does not implement `reply_to` — it falls back to plain send (same as previous behavior). Slack support for targeted replies can be added in a future PR.
