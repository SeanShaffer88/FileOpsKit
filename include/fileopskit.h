/* FileOpsKit.dll - file operation engine behind a small C ABI.
 *
 * Operations: copy, move, rename and delete jobs over local files
 * and directories, with a plan step, progress callbacks,
 * cooperative cancellation, four conflict policies, per-item
 * retries, and staged undo.
 *
 * Strings: all strings are UTF-8, NUL-terminated. Paths are
 * native host paths (on Windows, forward slashes work; the usual
 * Windows long-path rules of the underlying file APIs apply).
 *
 * Ownership: every char* result is caller-owned and must be
 * freed with fo_free_string(). fo_version() and fo_last_error()
 * return borrowed strings - do NOT free them.
 *
 * Errors: a NULL return means a hard failure (bad arguments,
 * unreadable spec or manifest); fo_last_error() /
 * fo_last_error_code() describe it on the calling thread.
 * Per-item job failures are NOT hard failures: fo_run() still
 * returns a result JSON whose "state" and per-item entries
 * report them.
 *
 * Threading: the API is synchronous - fo_run() does the work on
 * the calling thread and reports through the progress callback.
 * Distinct calls on distinct threads are independent except for
 * the thread-local last-error. Do not call back into fo_run /
 * fo_undo re-entrantly from a progress callback.
 */
#ifndef FILEOPSKIT_H
#define FILEOPSKIT_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
#if defined(_WIN32) && defined(FO_BUILD)
#define FO_API __declspec(dllexport)
#elif defined(_WIN32)
#define FO_API __declspec(dllimport)
#else
#define FO_API
#endif

#define FO_OK 0
#define FO_ERR_INVALID_ARG 1
#define FO_ERR_NOT_SUPPORTED 2
#define FO_ERR_NOT_FOUND 3
#define FO_ERR_ACCESS_DENIED 4
#define FO_ERR_PLATFORM 5
#define FO_ERR_INTERNAL 6
#define FO_ERR_IO 7
#define FO_ERR_CONFLICT 8
#define FO_ERR_CANCELLED 9

/* Library version, e.g. "0.1.0". Borrowed string; do not free. */
FO_API const char *fo_version(void);

/* Last error message on this thread ("" when none). Borrowed. */
FO_API const char *fo_last_error(void);

/* Last FO_ERR_* code on this thread (FO_OK when none). */
FO_API int fo_last_error_code(void);

/* Free a caller-owned string returned by this library.
 * NULL is accepted and does nothing. */
FO_API void fo_free_string(char *p);

/* Plan a job without touching anything.
 *
 * spec_json: one JSON object:
 *   {"op":"copy|move|rename|delete",
 *    "sources":["path", ...],                 (required, non-empty)
 *    "dest_dir":"dir",                         (copy/move: required)
 *    "dest":"path",                            (rename: required)
 *    "conflict":"fail|skip|overwrite|rename",  (default "fail")
 *    "retries":N, "retry_delay_ms":N,
 *    "undo_dir":"dir"}
 * Unknown keys are rejected; duplicate keys in one object are a
 * parse error. Directory sources are expanded recursively.
 *
 * Returns caller-owned plan JSON:
 *   {"op":..., "conflict_policy":..., "item_count":N,
 *    "total_bytes":N, "conflict_count":N,
 *    "items":[{"source":..., "dest":...|null, "bytes":N,
 *              "is_dir":bool, "conflict":bool}, ...]}
 * "conflict" is pre-detected: the destination already exists.
 * Returns NULL + last-error when the spec is invalid or a source
 * does not exist (FO_ERR_NOT_FOUND). */
FO_API char *fo_plan(const char *spec_json);

/* Progress callback for fo_run().
 * event_json (valid only for the duration of the call) is one
 * JSON object:
 *   {"phase":"item_start|chunk|item_done", "item_index":N,
 *    "item_count":N, "bytes_done":N, "total_bytes":N,
 *    "item_bytes_done":N, "item_bytes":N,
 *    "current_path":"...", "dest_path":"..."}
 * It fires once per item start, once per 64 KiB chunk while
 * copying a file, and once per item completion. Return 0 to
 * continue; return nonzero to CANCEL the job: the engine stops
 * promptly, a partially copied file is removed (a pre-existing
 * destination is left untouched), remaining items are reported
 * "cancelled", and the result state is "cancelled". Items that
 * already completed stay completed and stay listed - and stay
 * covered by the undo manifest. The callback must return
 * promptly; the engine cannot preempt host code. */
typedef int (*FoProgressFn)(void *user_data, const char *event_json);

/* Run a job (same spec JSON as fo_plan).
 *
 * Semantics:
 * - Directory sources recurse; the tree is mirrored under
 *   dest_dir/<source name>/ for copy/move.
 * - Move tries a plain rename first (same volume); across
 *   volumes it copies, then deletes the source only after the
 *   copy fully succeeds.
 * - Conflict policies:
 *     fail      - if ANY item conflicts, the job fails BEFORE
 *                 touching anything; state "failed".
 *     skip      - conflicting items are skipped and listed.
 *     overwrite - the old destination is staged into the undo
 *                 area before being replaced.
 *     rename    - the item lands at a unique "name (2).ext"
 *                 style destination, listed in the result.
 * - Retries: a failed item is retried up to "retries" extra
 *   times, waiting "retry_delay_ms" between attempts. The final
 *   failure is recorded per item and the job continues with the
 *   other items (state "completed_with_errors").
 * - Undo: a run that changes anything writes an undo manifest
 *   into the undo area ("undo_dir" from the spec, default
 *   <system temp>/fileopskit-undo-<id>) and stages overwritten
 *   and deleted files there. The result carries "undo_id" and
 *   "undo_manifest" (both null when nothing changed).
 *
 * Returns caller-owned result JSON:
 *   {"op":..., "state":"completed|completed_with_errors|
 *                        cancelled|failed",
 *    "item_count":N, "total_bytes":N, "bytes_done":N,
 *    "items":[{"source":..., "dest":...|null,
 *              "status":"done|skipped|failed|cancelled",
 *              "bytes":N, "attempts":N, "error":...|null}, ...],
 *    "undo_id":...|null, "undo_manifest":...|null}
 * progress_cb may be NULL (no progress, no cancellation). */
FO_API char *fo_run(const char *spec_json, FoProgressFn progress_cb,
                    void *user_data);

/* Reverse a run using its undo manifest.
 * manifest_path: the "undo_manifest" path from a run result.
 * Items are reversed in reverse order: copied files are
 * deleted, moved files are moved back, deleted and overwritten
 * files are restored from the staging area.
 *
 * Protection: undo of a copy (or of an overwritten destination)
 * refuses to touch a destination whose size or modification
 * time changed after the run - it is reported
 * "skipped_modified" and left exactly as it is. A move-back or
 * restore whose original location is occupied again is
 * reported "skipped_conflict".
 *
 * Returns caller-owned JSON:
 *   {"undo_id":..., "state":"completed|completed_with_errors",
 *    "items":[{"action":..., "source":...|null, "dest":...|null,
 *              "status":"done|failed|skipped_modified|
 *                        skipped_conflict",
 *              "error":...|null}, ...]}   (reverse order)
 * Returns NULL + last-error when the manifest cannot be read
 * or parsed. The manifest and staging area are left on disk;
 * deleting them after a successful undo is the caller's job. */
FO_API char *fo_undo(const char *manifest_path);

#ifdef __cplusplus
}
#endif
#endif
