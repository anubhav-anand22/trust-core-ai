# 11 · Conversations, sessions, and memory

The workbench is a chat: a conversation with a sidebar of past conversations,
where follow-up questions keep their context. This doc explains how a session is
stored, how a follow-up is answered, and where the two tiers of memory live.

## Two tiers, two lifetimes

```
 this conversation            across all conversations
 ─────────────────            ────────────────────────
 sessions/<id>.json           persistent_memory.json
 plain JSON                   AES-256-GCM, machine-derived key
 the transcript + a bounded   a compressed digest + (Stage 5)
 model-context summary        confirmed facts
```

`SessionContext` (`crates/workbench-core/src/memory/session.rs`) is one file per
chat. `PersistentMemory` (`persistent.rs`) is one encrypted file for the user,
folded in after every turn and on `end_session`.

## A session file

```jsonc
{
  "session_id": "s-abc123",
  "title": "What credit-card fee applies to a 1000 INR payment?",
  "created_at": 1725800000,
  "updated_at": 1725800420,

  // The full transcript — what the sidebar redraws when you reopen this chat.
  "exchanges": [
    { "prompt": "...", "attachments": ["fee_card.pdf"],
      "report": { "summary": "...", "findings": [...], ... }, "ts": 1725800180 }
  ],

  // The bounded view handed to the model. Older turns are compressed away.
  "turns":           [ { "user_prompt": "...", "plan_summary": "...", ... } ],
  "rolling_summary": "Earlier the user asked about MDR on credit cards (2%) ..."
}
```

Two things with different rules:

- **`exchanges`** grows without bound. Nothing is dropped; this is the record you
  see again.
- **`turns` + `rolling_summary`** are what the prompt actually carries. Once
  `turns.len() > MAX_VERBATIM_TURNS` (6), the oldest are folded into
  `rolling_summary` by a lightweight `summarize` call on the resident model. The
  prompt never grows past that budget no matter how long the chat runs.

`context_blob()` renders the summary plus the recent turns; that string is what
role A, role B and role C receive as "earlier in this conversation".

## Why one file per session

The pre-Stage-4 layout was a single `session_context.json` with **no session id
in the path**. Every chat wrote the same file, so:

- starting a new chat silently overwrote the previous one;
- a page reload minted a fresh id, and `load_or_new` — which filtered by id —
  then discarded the file and the next `save()` clobbered it.

`PipelineConfig::session_path_for(id)` now sanitises the id to a safe file stem
and returns `sessions/<stem>.json`. `SessionContext::list(dir)` is a `read_dir`
over that folder, newest-`updated_at` first, skipping unparseable files —
that is the sidebar's data. There is no filesystem plugin on the Tauri side, so
the frontend cannot read those files itself; every listing is a command
(`list_sessions` / `load_session` / `delete_session` / `rename_session`).

A `session_context.json` left over from before Stage 4 is migrated into
`sessions/` on the first `list_sessions`, then renamed `.json.migrated`.

## Follow-up questions

A turn like *"and what about debit cards?"* or *"show that as a table"* has no
new attachments and needs no knowledge base. Before Stage 4 the planner had
nothing valid to emit for it — every analysis task requires a preceding
extraction step — so the turn parked in the HITL modal.

Two changes fixed it:

1. **The planner sees the conversation.** `run_turn` loads the session *before*
   planning and passes `context_blob()` into role A and role B. A follow-up is
   now a plannable thing.
2. **`answer_followup`** — a `Stage::Retrieve` task (so it is valid as the only
   step) that runs the session transcript through the resident model with a
   "answer only from the conversation so far" instruction. Role B is told to use
   it as the whole plan for a follow-up; the deterministic fallback
   (`planner::fallback_plan`) emits it when `uploads.is_empty() &&
   !needs_knowledge && has_history`.

The tool reads `ToolContext::session_blob`, which the executor now threads
through from the loaded session.

## The turn, end to end (Stage 4 shape)

```
submit_turn(prompt, files, mode, session_id)
      │
      ▼
run_turn
  ├─ screen_uploads            drop unsupported types, warn
  ├─ SessionContext::load_or_new(sessions/<id>.json)     ← loaded up front now
  ├─ plan_turn(prompt, uploads, history, has_history)    ← role A + role B see history
  │     └─ retries exhausted → deterministic fallback (never a dead end)
  ├─ execute_plan(..., session_blob)                     ← answer_followup can read it
  ├─ assert_sane
  ├─ compile_report(prompt, results, session.context_blob(), facility.context_blob())
  └─ session.record_turn(exchange, turn)                 ← full Exchange + bounded summary
        └─ persist_long_term(session)                    ← encrypted digest, every turn
```

## Reading the log

| Line | Says |
|---|---|
| `app started sessionId=s-…` | which session the UI opened (from `localStorage`) |
| `submit_turn: start … session_id=s-…` | the turn's session |
| `plan validated` / `using a deterministic fallback plan` | which planner path ran |
| `session deleted` / `migrated legacy session_context.json` | session-management events |
| `session compression failed; keeping raw digest` | the summariser call failed; the raw digest is kept rather than lost |

## Persistent memory today

`persist_long_term` still only writes `history_digest` (a rolling prose summary),
and `facility_metadata` / `recurrent_tags` are structurally present but unwritten.
Turning those into a visible "what we remember about you" panel — with
**propose → you confirm** so a small model's mistake never becomes a permanent
fact — is Stage 5, not yet built.
