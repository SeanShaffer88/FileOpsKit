# FileOpsKit — agent notes

- The partial-file rule is load-bearing: a copy writes `<name>.fo-partial` next to the destination and is renamed into place only when complete. That's why cancellation can promise "a pre-existing destination is never torn" — a "simplification" that copies straight onto the destination breaks the cancel and overwrite guarantees together.
- Conflict `fail` is a *preflight*: the check runs against the finished plan before any item executes. Moving the check into the per-item loop silently turns it into "fail after some files already copied".
- Undo's size+mtime check on `created`/`overwritten` records is the user's protection against deleting their later edits. The mtime is captured after the final rename lands the copy; capturing it from the partial file or the source reintroduces false "modified" skips (or worse, false "unmodified" deletes).
- Staging order matters: for delete, the staged copy is size-verified *before* the original is removed; for overwrite, the old destination is staged *before* the partial is renamed over it. Reordering either one opens a window where the only copy is the one being destroyed.
- Move deletes the source only after the destination copy is complete (cross-volume path). The same-volume fast path is a plain rename and must stay attempted first — it is atomic and preserves metadata the chunked path does not.
- The JSON engine (src/json.rs) is the family design — duplicate keys in one object are a PARSE ERROR by design; do not "fix" that into last-wins.
- Gate: WinDevKit tools/gate.sh (fmt + clippy both targets + full tests). Check/clippy do not link — CI's real Windows build is the link gate.
