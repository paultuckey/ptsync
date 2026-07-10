# Design: `auto-tag` command

Status: **implemented** as a separate experimental crate,
`expiremental/auto-tagger` (a standalone `auto-tagger` binary, not a `ptsync`
subcommand). It lives outside the main crate on purpose: the heavier `image` +
`heic` decode stack and the vision-model call stay out of ptsync's small,
focused core. Built archive-centric per decision **D1** below — the queue keys on
the archive file, not `media_item`, so tagging needs only the archive and never
the original Takeout/iCloud zips. Sections that described the earlier DB-centric
draft are marked where they changed.

Generate a short caption and a set of tags for each photo/video using a vision
model, and write them into the media file's Markdown frontmatter. The job is
**resumable**: you can start it, quit, and pick up later without redoing work or
losing progress.

This doc reviews the proposed design, flags problems, and recommends a concrete
shape that fits the existing architecture.

---

## 1. Goals & non-goals

**Goals**

- Caption + tag every media item, written to its `.md` frontmatter.
- Resumable: interrupt (Ctrl-C, crash, laptop sleep) and resume with no lost or
  repeated work.
- Model-agnostic: local (Ollama, LM Studio, …) or cloud, chosen by config — see
  the model-flexibility research (OpenAI-compatible `/v1/chat/completions` as the
  single interface). Local is the default; cloud is opt-in.
- Honour the project's cardinal rules: **never lose a file or a note**, and
  **re-running produces no changes** (idempotent).

**Non-goals (for v1)**

- Face recognition / people (that's a separate feature; `people` already exists
  in frontmatter).
- Embedding-based semantic search (a later feature that can reuse these tags).
- Editing/curating tags in a UI.

---

## 2. How it fits the architecture

Two facts about the current codebase drive the whole design:

1. **`media_item.media_item_id` is `TEXT`** — a stable hash of the media path,
   not an autoincrement int (see `db_cmd.rs`, `DB_MEDIA_ITEM_CREATE`). Any FK to
   it must be `TEXT`.
2. **The database is a rebuildable cache; the Markdown archive is the source of
   truth.** The `db` command can be re-run and `--clear`ed at will, and there is
   no migration path — a schema-version bump forces existing users to rebuild
   (`db_prepare` in `db_cmd.rs`). So **durable tag data must live in the
   Markdown**, and the `auto_tags` table is only a *resumable work queue / cache*.

The consequence, which shapes everything below: **the Markdown frontmatter is
authoritative for "is this item tagged?"** The DB accelerates resume but must
never be the only place a result exists.

`markdown.rs` already merges array frontmatter fields idempotently (`people`,
`albums`, `original-paths` are unioned; the file is only rewritten if the
canonicalised frontmatter actually changed, and the user's note body below the
marker is preserved). `tags` slots straight into that machinery as a new unioned
array; `caption` as a scalar.

---

## 3. Review of the proposed design

Proposed table and flow:

```
auto_tags { auto_tags_id: int autoincrement, media_item_id: fk, date_started: dt, status: text, tags: text }
- pick a media row that has not had an attempt yet, then immediately insert a row with status "started"
- on completion set status "done" and populate tags; on error set status "error"
```

**The core is sound.** A status-per-item queue table is the standard resumable
job-queue pattern, and "claim then complete" is the right skeleton. But there are
a handful of problems — one of them directly breaks the "quit and resume"
requirement — plus several worthwhile improvements.

### 3.1 Problems

**P1 — The `started` state gets stuck on quit/crash (breaks resumability).**
This is the important one. If you quit (or crash, or sleep) while a row is
`started`, that row stays `started` forever. On the next run you must decide what
`started` means, and both naive answers are wrong:

- *Skip `started` rows* (treat as attempted) → any item interrupted mid-flight is
  **never tagged**. Since "quit then resume" is exactly the intended workflow,
  this silently drops items on every interruption.
- *Retry `started` rows* → fine for a single process, but unsafe if you ever run
  two workers (two workers both see it as retriable).

**Fix:** distinguish "claimed, in flight" from "permanently attempted", and
**reclaim stale `started` rows on startup**. Because ptsync is single-process
(Turso is single-writer and the code is not concurrent), the simplest correct
rule is: *at startup, any `started` row is a crashed remnant → requeue it.*
Combined with idempotent Markdown writes (P3), resume becomes trivially correct.

**P2 — "pick a row that has not had an attempt yet" conflates two tables.**
`auto_tags` starts empty, so there are no rows to pick. The queue is *derived*:
media items that have no `auto_tags` row yet.

```sql
SELECT mi.media_item_id
FROM media_item mi
LEFT JOIN auto_tags at ON at.media_item_id = mi.media_item_id
WHERE at.auto_tags_id IS NULL
  AND mi.kind IS NOT NULL          -- only real photos/videos
ORDER BY mi.guessed_datetime DESC  -- stable, predictable order
LIMIT 1;                           -- or a batch
```

**P3 — Claim + result write must be crash-ordered against the Markdown.**
The DB row is bookkeeping; the tags in the `.md` are the deliverable. If you set
`status = done` *before* writing the Markdown and crash in between, the item is
marked done but has no tags — lost. **Always write the Markdown first, then mark
`done`.** If it crashes after the write but before `done`, the next run re-tags
and the idempotent merge makes the rewrite a no-op. Safe either way.

**P4 — FK type.** `media_item_id` must be `TEXT`, not int.

**P5 — Select-then-insert is not atomic under concurrency.** If v1 ever grows a
`--concurrency` flag (see §7 — models are slow, ~1–30s/image, so you'll want it),
two workers can `SELECT` the same unclaimed row before either `INSERT`s. Claim in
a single writer transaction (`BEGIN IMMEDIATE`) or an atomic
`INSERT … SELECT … WHERE NOT EXISTS`. Single-process v1 is safe, but design the
claim so concurrency is a later flag, not a rewrite.

### 3.2 Improvements

- **I1 — `status` as opaque text loses failure detail.** Add `error_message TEXT`
  so a poison item can be diagnosed without re-running.
- **I2 — Retries need a bound.** A corrupt file or a model that always chokes on
  one image will loop forever if errors are retried. Add `attempts INTEGER` and
  stop retrying past a small cap; expose `--retry-errors` to opt back in.
- **I3 — Record the model and prompt version.** The whole point is model choice.
  Without `model TEXT` (and a `prompt_version`) you can't tell "tagged by a weak
  local model" from "tagged by a frontier model", nor re-tag selectively later.
- **I4 — Separate `caption` from `tags`.** The Tier-1 feature is *caption + tags*.
  Store them in separate columns; `tags` as a JSON array (clean to emit into the
  YAML list), `caption` as text.
- **I5 — Add `finished_at`.** Pair with `started_at` for throughput/ETA and to
  find slow items. (Rename `date_started` → `started_at` to match the existing
  `created_at` / `modified_at` / `run_date` convention.)

---

## 4. Data model (as built)

Archive-centric (D1): the queue keys on the archive-relative media path, with the
content hash carried alongside for reference. No FK to `media_item`.

```sql
CREATE TABLE IF NOT EXISTS auto_tags (
    auto_tags_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    archive_path   TEXT NOT NULL UNIQUE,     -- media path relative to the archive root
    checksum       TEXT,                     -- content hash from the .md frontmatter, if present
    status         TEXT NOT NULL,            -- 'started' | 'done' | 'error'
    caption        TEXT,                     -- one-line description, NULL until done
    tags           TEXT,                     -- JSON array of strings, NULL until done
    model          TEXT,                     -- model that produced the tags — provenance
    prompt_version INTEGER,                  -- bump when the prompt changes
    attempts       INTEGER NOT NULL DEFAULT 0,
    error_message  TEXT,                     -- populated on status='error'
    started_at     DATETIME DEFAULT CURRENT_TIMESTAMP,
    finished_at    DATETIME                  -- set on done/error
);
```

Notes:
- `UNIQUE(archive_path)` keeps the queue to one row per item and makes the claim a
  clean upsert (`INSERT … ON CONFLICT(archive_path) DO UPDATE`). Re-tagging =
  update the row + re-merge frontmatter.
- `tags` as JSON matches how it lands in YAML (`tags: [beach, sunset]`) and uses
  the existing `serde_json` dep.
- `checksum` is read opportunistically from the sidecar `.md` when we write it,
  purely for reference/debugging; the queue does not depend on it.

### Where the queue lives — and why there are *no* schema chores

The table is **self-contained** and lives in its own SQLite file, by default
`<output>/.ptsync/auto-tags.db` (override with `--db`). It is created on demand
with a plain `CREATE TABLE IF NOT EXISTS` in the `auto-tagger` crate.

This is the big payoff of the archive-centric choice: because `auto_tags` is *not*
part of the input-scan schema in `db_cmd.rs`, none of the schema-integration
chores the earlier draft listed apply — **no** `SCHEMA_TABLE_STATEMENTS` edit,
**no** `db_drop_all`/`--clear` entry, and crucially **no `DB_SCHEMA_VERSION`
bump**, so existing users are never forced to rebuild their scan database. Resume
state travels *with the archive*, independent of the working directory or the
input DB.

> The earlier draft put `auto_tags` in `db.sqlite` with a `media_item_id` FK and a
> version bump; D1 made that unnecessary.

---

## 5. CLI surface (as built)

Run the standalone binary (`cd expiremental/auto-tagger && cargo run -- …`):

```
auto-tagger --output <DIR> [OPTIONS]

  -o, --output <DIR>       Archive directory to tag (has the .md siblings)
      --db <FILE>          Queue/cache db [default: <output>/.ptsync/auto-tags.db]
      --base-url <URL>     OpenAI-compatible base URL [default: http://localhost:11434/v1]
      --model <NAME>       Vision model name [default: qwen2.5vl]
      --api-key-env <VAR>  Env var holding the API key (cloud only; local ignores it)
      --limit <N>          Tag at most N items this run (cost/time guardrail)
      --retry-errors       Re-attempt items currently in 'error'
      --retag              Re-tag items already 'done' (e.g. with a better model)
  -n, --dry-run            Report what would be tagged — no model call, no writes
  -d, --debug
```

Model connection is via flags for v1 (simple, scriptable), defaulting to a local
Ollama server so nothing leaves the machine unless the user opts in. The secret is
never a flag value: `--api-key-env NAME` names an env var to read, so the key
stays out of shell history and process listings. **Config-file profiles**
(`--profile local|openrouter|…` from the flexibility research) are a natural
follow-up that would wrap these same flags.

The command reads the image from the **archive** (`--output`) and writes tags to
the sidecar `.md` there, so tagging needs only the self-contained archive — never
the original Takeout/iCloud zips. (Decision D1, §9.)

---

## 6. Processing loop & state machine

```
                        ┌─────────── startup: requeue stale 'started' ───────────┐
                        v                                                         │
   (no row) ──claim──> started ──model ok──> [write .md] ──> done                │
      ^                   │                                                       │
      │                   └── model err / timeout ──> error (attempts++, message) ┘
      │                                                   │
      └───────────────── --retry-errors ─────────────────┘
```

Per run:

1. **Reclaim.** `DELETE FROM auto_tags WHERE status = 'started'` (or reset to a
   retriable state). Single-process ⇒ any `started` row is a crash remnant.
2. **Loop** until the queue is empty or `--limit` is hit:
   1. **Claim** one item (P2 query) in a writer transaction; insert
      `status='started', attempts = attempts+1`.
   2. **Skip-if-already-tagged:** resume authority is the queue row's `status`
      (`done`/`error` are excluded from the pending set unless `--retag` /
      `--retry-errors`). Because the queue db is self-contained and never
      force-wiped (§4), this is durable across runs. *(A belt-and-braces check of
      the `.md` frontmatter itself is a possible hardening; not in v1.)*
   3. **Read** the image bytes from the archive. *(v1 sends them as-is;
      downscaling to a ~1024px long edge is a deferred optimisation — §11.)*
   4. **Call** the model (OpenAI-compatible chat/completions, image as base64
      data URI). Ask for strict JSON: `{ "caption": "...", "tags": ["...", …] }`.
   5. **On success:** write frontmatter first (`markdown.rs` idempotent merge),
      *then* `UPDATE auto_tags SET status='done', caption=?, tags=?, model=?,
      finished_at=CURRENT_TIMESTAMP`.
   6. **On error:** `UPDATE … SET status='error', error_message=?,
      finished_at=…`. Never abort the batch for one bad item.
3. **Checkpoint** the WAL on exit (`PRAGMA wal_checkpoint(TRUNCATE)`) as the other
   commands do, so `db.sqlite` stays a single directly-readable file.

**Ctrl-C:** install a handler that sets a stop flag; finish the in-flight item
(or let its `started` row be reclaimed next run) and exit cleanly after the
current DB write. Either way, correctness rests on reclaim + idempotent writes,
not on catching the signal perfectly.

---

## 7. Concurrency & throughput

Vision inference is slow — research puts local 7B captioning around ~30s/image,
so a 20k-photo library is *days* serial. Design the claim for concurrency now,
even if v1 defaults to 1:

- Model calls are I/O-bound HTTP; run `--concurrency N` of them in parallel
  (bounded worker pool). DB writes serialize through the single Turso writer —
  fine, they're microseconds next to a 30s model call.
- With N>1 the claim **must** be atomic (P5): claim inside `BEGIN IMMEDIATE`, or
  `INSERT … SELECT … WHERE NOT EXISTS(…) RETURNING`.
- **Cloud rate limits:** exponential backoff on 429/5xx; treat as retryable
  (don't burn an `attempts`), distinct from a genuine content error.

`indicatif` is already a dep — show `done / total`, error count, and ETA; the
queue counts (`GROUP BY status`) make this a cheap query.

---

## 8. Write-back to Markdown

- New frontmatter fields: `tags: [..]` (unioned array — reuses
  `yaml_array_merge`) and `caption:` (scalar). Optionally `tagged-by:` /
  `tagged-model:` for provenance, or keep provenance DB-only.
- The merge only rewrites the file if the canonical frontmatter changed and
  **preserves the user's note body** below the marker — so this stays additive
  and idempotent, satisfying "re-running produces no changes".
- If a media item maps to more than one archive file after dedup, tag the single
  archive file once (keyed by the archive path / content hash), not per input
  duplicate.

---

## 9. Open decisions

- **D1 — Image source: archive vs input.** **RESOLVED: archive** (`--output`), so
  tagging never needs the original zips again. This drove the archive-centric data
  model in §4.
- **D2 — Tag vocabulary.** **Done: free-form, normalised (lowercase + dedupe) on
  parse.** A controlled vocabulary remains a possible later refinement.
- **D3 — Language.** The project targets English/Mandarin/Hindi/Spanish. Add a
  `--lang` (or per-profile prompt) so captions/tags can be generated in the
  user's language. *(Not in v1.)*
- **D4 — Provenance in Markdown or DB only?** **Done: DB only** (`model`,
  `prompt_version` on the row). Frontmatter stays clean.
- **D5 — Prompt/response contract.** **Done: tolerant parser** — request strict
  JSON, then slice to the outermost `{…}` before parsing so code fences/prose
  around the object don't break it. A "JSON only" retry or `response_format`
  fast-path could harden it further.

---

## 10. Summary of changes from the original proposal

| Original | Change | Why |
|---|---|---|
| `media_item_id` int FK | **`archive_path` (TEXT, UNIQUE) + `checksum`** | D1: archive-centric; self-contained, no `media_item` FK, no schema-version bump |
| `started` skipped/ambiguous on resume | **Requeue `started` on startup** | Otherwise interrupted items are never tagged — breaks "quit & resume" |
| set `done` then (implicitly) write tags | **Write Markdown first, then `done`** | Crash-safety; DB is a cache, Markdown is truth |
| `tags: text` only | split `caption` + `tags` (JSON) | matches the caption+tags feature and YAML output |
| — | add `model`, `prompt_version`, `attempts`, `error_message`, `finished_at` | provenance, bounded retries, diagnosability |
| single-shot select-then-insert | upsert claim (`ON CONFLICT`) | one row per item; safe to extend to concurrency later |
| — | reclaim + idempotent writes as the resume contract | correctness independent of catching Ctrl-C |

---

## 11. Image pipeline (as built) & remaining follow-ups

**Preprocessing (done).** Every image is decoded, downscaled so its longest edge
is **786px** (never upscaled), and re-encoded as a quality-85 **JPEG** before
being sent — so the model always receives a small JPEG regardless of source
format, which cuts latency and cloud cost sharply (a 2.3 MB iPhone HEIC becomes a
~48 KB JPEG). Decoders:
- **JPG/PNG/GIF** via the [`image`](https://crates.io/crates/image) crate.
- **HEIC** via the pure-Rust [`heic`](https://crates.io/crates/heic) crate — no
  libheif system dependency, so `cargo build` stays self-contained. It handles
  iPhone grid HEVC-I HEICs; the ~27% of the HEIF corpus it can't decode surface
  as per-item `error` rows, not crashes. (`0.2.0` is yanked; pinned to `0.1`.)

Remaining follow-ups:
- **Video still skipped.** MP4/MOV need a frame-extraction step (e.g. ffmpeg)
  before a vision model can read them.
- **Serial (concurrency = 1).** The upsert-based claim is designed so a
  `--concurrency N` worker pool can be added without reworking the schema; §7.
- **Model-config flags, not profiles.** A `--profile` config file (§5) is the
  planned ergonomic layer over the current `--base-url`/`--model`/`--api-key-env`.
- **No graceful Ctrl-C handler.** Not needed for correctness — an interrupted
  `started` row is reclaimed next run — but finishing the in-flight item on
  SIGINT would be tidier.
