# FileOpsKit — Parity (spec vs implementation)

Evidence: the Rust test suite in `src/tests.rs` and `src/ffi.rs`
(24 tests, all passing at the v0.1.0 gate) plus the header
contract in `include/fileopskit.h`. "Verified" below means a
named test exercises the behaviour on real temporary
directories.

| Spec feature | Status | Evidence |
|---|---|---|
| Copy jobs (files + recursed directory trees) | Verified | `copy_tree_preserves_contents`, `plan_expands_tree_and_detects_conflicts` |
| Move jobs (source removed only after success; same-volume rename fast path) | Verified | `move_removes_source_after_success` (+ undo move-back in the same test) |
| Rename (single source to an exact `dest`) | Verified | `rename_single_file` |
| Delete jobs | Verified | `delete_then_undo_restores` |
| Consistent interface across local drives / network shares / other providers | Partial | One engine over `std::fs`, so OS-mounted shares behave identically to local drives by construction. No pluggable provider layer exists yet — see ROADMAP item 2; not claimed as done. |
| `fo_plan`: expanded items, per-item dest + bytes, totals, conflicts pre-detected | Verified | `plan_expands_tree_and_detects_conflicts` (item count, total bytes, conflict count) |
| Progress reporting (per item start, per 64 KiB chunk, per item done; bytes done/total) | Verified | `progress_reports_monotonic_bytes`; chunk events asserted in `cancel_via_callback_stops_and_reports` |
| Cancellation via callback (prompt stop, state `cancelled`, completed items stay listed, partial copy removed, existing dest untorn) | Verified | `cancel_via_callback_stops_and_reports`, FFI `ffi_run_with_cancelling_callback` |
| Conflict `fail`: whole job fails before touching anything | Verified | `fail_policy_preflight_touches_nothing` (non-conflicting item also untouched, no manifest) |
| Conflict `skip`: conflicting items skipped and listed | Verified | `skip_policy_leaves_conflicts_alone` |
| Conflict `overwrite`: dest backed up into the undo area first | Verified | `overwrite_stages_original_and_undo_restores_it` |
| Conflict `rename`: unique `name (2).ext` destination, listed in result | Verified | `rename_conflict_picks_unique_name` (lands at `a (3).txt`), `unique_dest_naming` |
| Retries: per-item, up to N with delay, final failure recorded, job continues | Verified | `retries_are_counted_per_item` (attempts = retries+1, state `completed_with_errors`); recovery path in `retry_helper_recovers_from_flaky_failures` |
| Result JSON: per-item status + overall state (`completed` / `completed_with_errors` / `cancelled` / `failed`) | Verified | asserted across the run tests above |
| Undo manifest written into `undo_dir` (default system temp) for every run that changes anything; result carries `undo_id` + `undo_manifest` | Verified | manifest path asserted in copy/move/delete tests; default-temp path is the same code path with a different root (not separately asserted) |
| `fo_undo`: reverse order — copies deleted, moves moved back, deleted/overwritten restored from staging | Verified | `move_removes_source_after_success`, `delete_then_undo_restores`, `overwrite_stages_original_and_undo_restores_it`, `undo_copy_removes_unmodified_dest` |
| Undo of a copy must not delete a dest modified after the run (size + mtime check) | Verified | `undo_copy_refuses_to_delete_modified_dest` (`skipped_modified`, file untouched) |
| Move across volumes = copy + delete source after success | Implemented, not directly verified | The rename-failure fallback in `run_one` implements it; the test environment has a single volume, so no test forces the fallback. Stated honestly rather than claimed. |
| Header documents every export: semantics, error codes, cancellation and lifetime rules | Done | `include/fileopskit.h` (ownership, NULL/error contract, callback contract, undo protection) |

## Known gaps (also in README "Limitations")
- Empty directories are not mirrored on copy.
- Copies preserve contents only — no timestamps/permissions, no fsync in v0.1.
- No provider/plugin interface yet (local + OS-mounted filesystems only).
