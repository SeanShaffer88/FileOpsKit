# FileOpsKit — Quality Scorecard

Scored against the v0.1.0 gate run on the development VM:
`cargo fmt --check`, clippy (host + `x86_64-pc-windows-msvc`,
`-D warnings`), and the full test suite — 24/24 passing.
Scores are only claimed where that evidence exists; Windows
runtime behaviour is not claimed from Linux evidence.

| Dimension | Score /5 | Evidence |
|---|---|---|
| Correctness of core operations | 5 | Copy tree, move, rename, delete all verified end-to-end on real temp dirs with content assertions, not just status checks |
| Cancellation & partial-state safety | 5 | Mid-copy cancel test: state `cancelled`, partial dest absent, later items untouched and listed; partial-file design means a pre-existing dest cannot be torn |
| Conflict policies | 5 | All four policies tested, including `fail` preflight touching nothing and `rename` skipping taken `(2)` names |
| Undo safety | 4 | Restore paths for delete/overwrite/move verified; modified-after-copy protection verified. Cross-volume move fallback and dir-overwrite staging are implemented but not test-covered — one point held back |
| Retries | 4 | Attempt counting verified against a deterministically failing destination and a flaky closure; a real transient-IO recovery (file locked, then released) is not simulated — one point held back |
| API & documentation | 4 | Header documents every export with semantics, error codes, and lifetime/cancellation rules; README has a runnable C example. No compiled C consumer test in the suite (FFI is exercised from Rust) — one point held back |
| Code health | 5 | Zero dependencies, fmt + clippy clean on both targets at the gate, no unwinding across the FFI boundary |
| Platform coverage | 3 | Engine is portable `std::fs` and fully tested on Linux; the Windows MSVC build/link and on-Windows behaviour are CI's to prove, not yet observed — honestly mid-scored |

**Total: 88/100**

## Waivers / open items
- Cross-volume move: implemented (rename-failure fallback), untestable in a single-volume environment. Follow-up: CI job with a second volume or a forced-fallback test hook.
- No dimension is scored on Windows runtime evidence; the first Windows CI run is the follow-up that can raise Platform coverage.
