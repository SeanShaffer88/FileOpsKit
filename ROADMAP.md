# FileOpsKit Roadmap

## v0.1.0 (current)
The file operation engine behind a C ABI: JSON job specs for copy, move, rename and delete; a plan step that expands directory sources and pre-detects conflicts; progress callbacks (item start, per-64 KiB chunk, item done) whose nonzero return cancels promptly; four conflict policies (`fail` as a true preflight, `skip`, `overwrite`, `rename` with `name (2).ext` unique naming); per-item retries with recorded attempt counts; and staged undo — a manifest plus staged copies of overwritten/deleted files, reversed in reverse order, with a size+mtime guard that refuses to delete or restore over a destination edited after the run.

## Next, in order
1. **Metadata preservation**: optional preservation of modification times (and read-only flags) on copies, recorded in the plan so hosts can choose per job.
2. **Provider interface**: a narrow internal provider trait so archives and network locations can be planned/run through the same spec and result shapes the local engine uses today — the File Provider Adapter direction, landed only when a second real provider exists to prove the shape.
3. **Durability option**: an opt-in fsync of copied files and their containing directory for backup-grade jobs, off by default (it is meaningfully slower).
4. **Empty-directory mirroring**: plan entries for directories so empty folders survive a copy, and undo can restore a deleted tree's shape exactly.
5. **Undo expiry helper**: a small call to list/inspect undo areas and delete one explicitly, so hosts can offer "clear undo history" without walking the temp folder themselves.

## Explicitly not planned
- A job queue or scheduler: FileOpsKit runs one job correctly; queuing is the host app's policy.
- Silent overwrite without staging: `overwrite` always stages the old file first — a mode that destroys the only copy with no undo is not offered.
- Shell integration (recycle bin, Explorer progress UI): delete here means delete, staged by the undo area; shell behaviours belong to a shell-interop layer, not the engine.
