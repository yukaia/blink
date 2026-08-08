# Checkpoint Resume Offer

**Date:** 2026-08-08  
**Status:** Approved  

## Summary

Offer to resume an interrupted transfer batch when connecting to a session that has a checkpoint on disk. Today the checkpoint is written, kept, and never surfaced: the only ways to reach it are `r` / `R` in the Transfers pane (undiscoverable unless you already know) and the `blink checkpoints` CLI. A modal after connect shows a short summary and offers to resume, discard, or defer.

The transfer machinery already exists — `App::resume_walk` does the whole job. This work is discovery, a panel, and sequencing.

## Scope

- **In scope:** a post-connect modal per pending checkpoint; a summary (direction, counts, age, sample paths); resume / discard / later; discard sweeping orphaned `.part` files; clearing stale checkpoint state on disconnect (a prerequisite, see below).
- **Out of scope:** byte totals in the summary (not stored; would need another format version and would be stale anyway); a config option to suppress the panel; changing how checkpoints are keyed; resuming a checkpoint belonging to a *different* session than the one connected.

## Prerequisite: disconnect leaves checkpoint state behind

`App::disconnect` clears `transfer_manager`, `current_session`, `pending_password`, and the pending-modal fields, but not `active_checkpoints` or `checkpoint_job_map`. This predates the feature and has to be fixed for it to work at all.

Two failures follow from it:

1. Disconnect mid-batch, reconnect, and the panel offers a resume that `resume_walk` immediately refuses — its "a batch is still in flight" guard reads the stale in-memory checkpoint.
2. Worse, and independent of this feature: a new `TransferManager` restarts job ids at 1, so a fresh job's id can collide with a stale `checkpoint_job_map` entry and mark the *wrong* checkpoint entry done.

`disconnect` must clear both fields. A regression test covers it.

## Architecture

### Discovery

```rust
// checkpoint.rs
pub struct CheckpointOffer {
    pub kind: CheckpointKind,
    pub session: String,
    pub remaining: usize,
    pub total: usize,       // remaining + done; Cancelled excluded
    pub age: Option<Duration>,
    pub sample_paths: Vec<String>,   // display-only, already sanitized
}

pub fn offers_for(session: &str) -> Vec<CheckpointOffer>;
```

`offers_for` loads both kinds and keeps those with `pending_count() > 0`. Both loads already return `Ok(None)` for a missing file, so the common case is two cheap reads.

`total` is `remaining + done`, not `jobs.len()`. A `Cancelled` job is work the user already abandoned, so counting it in the denominator would overstate what is left to do.

`age` comes from the checkpoint file's mtime. `Checkpoint::load` doesn't expose the path it read, so this needs a small internal helper alongside `path_for`.

`sample_paths` holds up to three paths from jobs where `needs_resume()`, taken from the **source** side — `remote_path` for a download, `local_path` for an upload — because that is what the user selected and will recognise. They are sanitized at construction: remote paths carry the server's own bytes (see `RemoteEntry`), and this struct feeds a renderer that draws spans directly rather than going through `push_log`, which is where sanitization otherwise happens centrally.

### Sequencing

```rust
// tui/state.rs
pub enum PostConnectOffer {
    ResumeCheckpoint(CheckpointOffer),
    SaveSession,
}

// App
pending_offers: VecDeque<PostConnectOffer>,
```

`Connected` fills the queue — checkpoints first, then `SaveSession` if `pending_session_unsaved` — and calls `show_next_offer()`:

```rust
fn show_next_offer(&mut self) {
    self.screen = match self.pending_offers.front() {
        Some(PostConnectOffer::ResumeCheckpoint(_)) => Screen::OfferResumeCheckpoint,
        Some(PostConnectOffer::SaveSession) => Screen::OfferSaveSession,
        None => Screen::Main,
    };
}
```

The offer stays at the front of the queue while it is displayed, and is popped when answered. The queue is the single owner of "what happens after connect"; `handle_offer_save_session` changes to pop and advance rather than hardcoding `Screen::Main`.

Checkpoints come first because they are keyed by session *name*, and accepting the save offer can rename the session. Resuming first means the resume runs under the name the checkpoint was loaded with. Once dispatched, the active checkpoint carries its own `session` field, so it settles correctly even if the name changes afterwards.

### Flow

```
Connected
  ├─ offers_for(session.name)      → [download ckpt, upload ckpt]
  ├─ + SaveSession if unsaved      → [dl, ul, save]
  └─ show_next_offer()             → Screen::OfferResumeCheckpoint

  [r] resume  → resume_walk(kind); pop; show_next_offer()
  [d] discard → discard(session, kind); pop; show_next_offer()
  [esc] later → pop; show_next_offer()          (file untouched)
  other       → ignored
                                   → next offer, or Screen::Main
```

One panel per checkpoint, shown in turn. The view never renders a list, and per-direction choice falls out for free: two prompts in the uncommon case where both directions have pending work.

Unlisted keys are ignored rather than treated as a default, matching every other confirm modal in the app.

### Actions

**Resume** delegates to the existing `App::resume_walk(Direction)`, which reloads from disk, filters to `needs_resume()`, and re-queues through `dispatch_plan`. Its "batch already in flight" guard is satisfied by definition at connect time — given the disconnect fix above.

**Discard** composes in `checkpoint.rs` so the CLI can share it:

```rust
pub struct DiscardOutcome {
    pub parts_removed: usize,
    pub failures: Vec<String>,
}

pub fn discard(session: &str, kind: CheckpointKind) -> Result<DiscardOutcome>;
```

It loads the checkpoint, sweeps its orphaned partials, then removes the file. Idempotent: a checkpoint that is already gone is `Ok` with nothing removed, matching `Checkpoint::remove`. Sweeping matters because the checkpoint is the only record of where those `.part` files are — delete it without sweeping and they are stranded forever. `remove_orphan_parts` already skips `Done` jobs, so only partials of transfers that never finished are removed.

`remove_orphan_parts` currently reports failures with `eprintln!`. That is right for the CLI and wrong for the TUI, where writing to stderr smears the display. It changes to return failures; `list_and_clean` prints them, the panel logs them.

## Panel

Modelled on `views::offer_save_session` — same `centered_rect`, block, and key-hint conventions.

```
┌─ resume interrupted transfer? ───────────────────┐
│                                                  │
│   an interrupted download batch was found        │
│                                                  │
│   12 of 40 items remaining · 3 hours ago         │
│                                                  │
│   /srv/photos/2026/IMG_0431.CR2                  │
│   /srv/photos/2026/IMG_0432.CR2                  │
│   /srv/photos/2026/IMG_0433.CR2                  │
│                                                  │
│   [r] resume   [d] discard   [esc] later         │
└──────────────────────────────────────────────────┘
```

Age is rendered coarsely ("3 hours ago", "yesterday", "6 days ago"). Sample paths are truncated middle-out to the panel width, reusing `truncate_middle`.

## Error Handling

Connecting must never fail because of a checkpoint.

| Condition | Behaviour |
|---|---|
| Checkpoint file absent | No offer. |
| File won't parse, or version is newer than supported | Skipped, warning logged. `offers_for` returns only what loaded cleanly. |
| `pending_count() == 0` | No offer. Already self-deleting on completion. |
| Discard fails (permissions) | Error logged, queue still advances — the user is never trapped in the modal. |
| A `.part` sweep fails | Reported per file through the log; the checkpoint is still removed. |
| Local directories gone since the batch | Unchanged behaviour: the download worker `create_dir_all`s; a missing upload source fails that one job. |
| Quit mid-queue | Remaining checkpoints stay on disk — same as answering "later". |

## Known Consequence: name-keyed checkpoints

Checkpoints are keyed by session name only, not host. `blink connect sftp://host` builds an ad-hoc session named `host`, so it can be offered a checkpoint belonging to a *saved* session of the same name — whose paths may refer to a different server. Editing a saved session's host has the same effect.

Accepted rather than fixed here. Storing host and port in the checkpoint and warning on mismatch is a reasonable follow-up; it is additive to format version 3 and would not need a version bump. Documented in the README instead.

## Testing

**`checkpoint.rs`**

- `offers_for` returns nothing when no files exist.
- Returns one offer for a checkpoint with pending work, none for a fully-done one, two when both directions have work.
- `remaining` is `pending_count()`, `total` is `remaining + done_count()`. A checkpoint holding `Cancelled` jobs reports a denominator that excludes them.
- `sample_paths` is capped at three, drawn from the source side, and sanitized — a job path carrying a control character must not survive into the offer.
- A checkpoint that fails to parse is skipped rather than propagating.
- `discard` removes the file and sweeps partials of non-`Done` jobs, leaving a `Done` job's file alone.

**App level**

- `Connected` builds `[checkpoint…, save]` in that order.
- `show_next_offer` walks the queue; an empty queue lands on `Screen::Main`.
- `r` enqueues the remaining jobs; `d` removes the file; `esc` leaves it on disk.
- A plan containing `Mkdir` jobs counts them as items, so the denominator matches the plan rather than only its file transfers.
- Keys the panel doesn't list do not dismiss it.
- `disconnect` clears `active_checkpoints` and `checkpoint_job_map` (regression, see Prerequisite).

The view is not unit-tested. There is no terminal harness in this repo and the existing modals are likewise untested at the render level.

New checkpoint tests must not leave files in the user's real checkpoint directory. The existing App-level checkpoint tests already write there and clean up only on success; this is the point at which to route the checkpoint directory to a temp path under test.

## README Update

- Document the panel in the walk-checkpointing section, including that "later" defers and "discard" also removes partial downloads.
- Note the name-keying consequence above.
- Fix the stale `checkpoint_glue.rs` line (499), which still names `discard_active_checkpoint` — renamed to `cancel_batch_in_checkpoint` / `settle_checkpoint` during the audit work.

## Files Changed

| File | Change |
|---|---|
| `src/checkpoint.rs` | `CheckpointOffer`, `offers_for`, age helper, `discard`, `DiscardOutcome`; `remove_orphan_parts` returns instead of printing |
| `src/tui/state.rs` | `PostConnectOffer` |
| `src/tui/app/mod.rs` | `Screen::OfferResumeCheckpoint`, `pending_offers`, `draw` / `handle_key` arms, `show_next_offer`, `disconnect` fix |
| `src/tui/app/handlers.rs` | `handle_offer_resume_checkpoint`; `handle_offer_save_session` pops and advances |
| `src/tui/app/events.rs` | `Connected` builds the offer queue |
| `src/tui/views.rs` | `offer_resume_checkpoint` module |
| `README.md` | panel docs, name-keying note, stale reference fix |
