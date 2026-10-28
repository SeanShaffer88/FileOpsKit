# FileOpsKit

## TL;DR

FileOpsKit does the boring-but-dangerous part of file management — copying, moving, renaming and deleting batches of files — behind one small DLL call. You describe the job in plain JSON, it first shows you a plan (every file, where it will land, how big, and which destinations are already taken), then does the work while reporting progress, letting you cancel, handling name clashes the way you chose, retrying hiccups, and writing an "undo" record so a finished job can be rolled back.

```c
#include "fileopskit.h"
#include <stdio.h>

static int on_progress(void *ctx, const char *event_json) {
    printf("%s\n", event_json);
    return 0; /* nonzero = cancel the job */
}

int main(void) {
    char *plan = fo_plan("{\"op\":\"copy\",\"sources\":[\"C:/photos\"],"
                         "\"dest_dir\":\"D:/backup\",\"conflict\":\"rename\"}");
    printf("%s\n", plan);          /* inspect before touching anything */
    fo_free_string(plan);

    char *result = fo_run("{\"op\":\"copy\",\"sources\":[\"C:/photos\"],"
                          "\"dest_dir\":\"D:/backup\",\"conflict\":\"rename\"}",
                          on_progress, NULL);
    printf("%s\n", result);        /* per-item status + undo_manifest path */
    fo_free_string(result);
    return 0;
}
```

## Key features

- **Plan before you touch anything.** `fo_plan` expands directory sources recursively and reports every item: source, destination, bytes, total size, and which destinations already exist. A job with the `fail` conflict policy uses exactly this plan to refuse a conflicted job *before* changing a single file.
- **Four operations, one spec.** `copy`, `move`, `rename` (single item) and `delete` all take the same JSON shape. Directory sources recurse; the tree is mirrored under `dest_dir/<source name>/`.
- **Progress and real cancellation.** The callback fires per item start, per 64 KiB chunk copied, and per item completion, with bytes done out of total. Returning nonzero cancels promptly: a half-copied file is removed, a pre-existing destination is never torn (copies land via a partial file that is only renamed into place when complete), completed items stay listed in the result, and remaining items are reported `cancelled`.
- **Conflict policies with exact meanings.** `fail` (job fails up front, nothing touched), `skip` (conflicting items skipped and listed), `overwrite` (the old file is staged into the undo area first), `rename` (the item lands at a unique `name (2).ext`-style path, listed in the result).
- **Retries per item.** A failed item is retried up to `retries` extra times with `retry_delay_ms` between attempts; the result records the real attempt count. One bad item does not kill the job — unless the policy is `fail` — and the overall state becomes `completed_with_errors`.
- **Undo that respects later edits.** Every run that changes anything writes an undo manifest and stages overwritten/deleted files. `fo_undo` reverses in reverse order: copies deleted, moves moved back, deletions and overwrites restored. If a copied file was edited after the run (size or modification time differs from the manifest), undo skips it and says so instead of deleting someone's newer work.
- **Honest result JSON.** Every item ends `done`, `skipped`, `failed` or `cancelled`, with bytes and attempts — no silent partial state.
- **Zero dependencies.** Hand-rolled JSON (duplicate keys in one object are a parse error, by design), plain `std::fs` engine, small C ABI.

## The API in one screen

| Call | What it does |
|---|---|
| `fo_plan(spec_json)` | Expand + preflight a job; returns plan JSON |
| `fo_run(spec_json, progress_cb, user_data)` | Run the job; returns result JSON |
| `fo_undo(manifest_path)` | Reverse a run from its manifest; returns undo result JSON |
| `fo_version` / `fo_last_error` / `fo_last_error_code` / `fo_free_string` | Version, thread-local error, string ownership |

Job states: `completed`, `completed_with_errors`, `cancelled`, `failed`. Hard failures (bad spec, missing source, unreadable manifest) return NULL and set the thread-local error instead.

## Build

```sh
cargo build --release   # file_ops_kit.dll / .so / .a + include/fileopskit.h
cargo test              # full test suite
```

Rust 1.80+, edition 2021, no dependencies.

## Limitations, honestly

- **Local filesystems only in this version.** The engine is `std::fs`: local drives and mounted network shares work through the operating system, but there is no pluggable provider layer (no archives, no cloud APIs) — that is roadmap, and the spec/plan/result shapes are designed to admit it later.
- **Empty directories are not mirrored.** Planning expands files; a source tree's empty folders do not appear at the destination. For move/delete, source directories are removed only once empty.
- **Undo cannot resurrect everything.** Undo protects destinations edited after the run by skipping them; a staged restore whose original spot is occupied again is also skipped and reported. Undo of a copied file that was *deleted* by hand counts as done (nothing to do). Directory renames staged by overwrite are staged by rename, which needs the undo area on a filesystem where that rename works.
- **Timestamps and permissions are not preserved** on copies in v0.1 (contents only), and copies are not fsynced — a power cut mid-copy can still lose the in-flight file, though never tear a pre-existing destination.
- **Synchronous.** `fo_run` works on the calling thread; hosts that need a worker thread run the call on one of theirs.
- **Long paths / Unicode:** paths travel as UTF-8; on Windows the behaviour of the underlying APIs applies (notably the classic `MAX_PATH` limits unless the host process is long-path aware).

## Tests

24 tests over real temporary directories: plan expansion and conflict pre-detection, copy trees, move + undo move-back, single rename, delete + undo restore, overwrite + undo restore, `rename`-policy unique naming (`a (3).txt` when `a (2).txt` is taken), `skip`, `fail` preflight touching nothing, cancellation mid-copy via the callback, retry attempt counting (engine-level and the retry helper against a flaky closure), modified-after-copy undo protection, monotonic progress bytes, spec validation, and FFI end-to-end including a cancelling C callback.

## License

MIT. Part of the WinDevKit family — see `AGENTS.md` for build discipline and `ROADMAP.md` for what is next.
