//! FileOpsKit — file operation engine core.
//!
//! Pure `std::fs` engine behind the C ABI in `ffi`: copy, move,
//! rename and delete jobs with planning, progress callbacks,
//! cancellation, conflict policies, per-item retries and staged
//! undo. The engine is platform-independent Rust; paths are the
//! host's native paths carried as UTF-8 strings in JSON.

pub mod ffi;
pub mod json;
pub mod platform;

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/fileopskit.h).
pub const FO_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_SUPPORTED: i32 = 2;
pub const ERR_NOT_FOUND: i32 = 3;
pub const ERR_ACCESS_DENIED: i32 = 4;
pub const ERR_PLATFORM: i32 = 5;
pub const ERR_INTERNAL: i32 = 6;
pub const ERR_IO: i32 = 7;
pub const ERR_CONFLICT: i32 = 8;
pub const ERR_CANCELLED: i32 = 9;

pub type FoResult<T> = Result<T, (i32, String)>;

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use json::Value;

pub const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Copy,
    Move,
    Rename,
    Delete,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Copy => "copy",
            Op::Move => "move",
            Op::Rename => "rename",
            Op::Delete => "delete",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Fail,
    Skip,
    Overwrite,
    Rename,
}

impl Conflict {
    pub fn name(self) -> &'static str {
        match self {
            Conflict::Fail => "fail",
            Conflict::Skip => "skip",
            Conflict::Overwrite => "overwrite",
            Conflict::Rename => "rename",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub op: Op,
    pub sources: Vec<PathBuf>,
    pub dest_dir: Option<PathBuf>,
    pub dest: Option<PathBuf>,
    pub conflict: Conflict,
    pub retries: u32,
    pub retry_delay_ms: u64,
    pub undo_dir: Option<PathBuf>,
}

/// Parse and validate a job spec JSON document.
/// Unknown keys are rejected; duplicate keys are a parse error
/// (enforced by the JSON parser).
pub fn parse_spec(text: &str) -> FoResult<Spec> {
    let v = json::parse(text).map_err(|e| (ERR_INVALID_ARG, format!("invalid spec JSON: {e}")))?;
    let pairs = v
        .as_object()
        .ok_or((ERR_INVALID_ARG, "spec must be a JSON object".to_string()))?;
    const KNOWN: &[&str] = &[
        "op",
        "sources",
        "dest_dir",
        "dest",
        "conflict",
        "retries",
        "retry_delay_ms",
        "undo_dir",
    ];
    for (k, _) in pairs {
        if !KNOWN.contains(&k.as_str()) {
            return Err((ERR_INVALID_ARG, format!("unknown spec key \"{k}\"")));
        }
    }
    let op = match v.get("op").and_then(Value::as_str) {
        Some("copy") => Op::Copy,
        Some("move") => Op::Move,
        Some("rename") => Op::Rename,
        Some("delete") => Op::Delete,
        Some(other) => return Err((ERR_INVALID_ARG, format!("unknown op \"{other}\""))),
        None => return Err((ERR_INVALID_ARG, "spec requires \"op\"".to_string())),
    };
    let src_vals = v.get("sources").and_then(Value::as_array).ok_or((
        ERR_INVALID_ARG,
        "spec requires \"sources\" array".to_string(),
    ))?;
    if src_vals.is_empty() {
        return Err((ERR_INVALID_ARG, "\"sources\" must not be empty".to_string()));
    }
    let mut sources = Vec::new();
    for sv in src_vals {
        let s = sv
            .as_str()
            .ok_or((ERR_INVALID_ARG, "every source must be a string".to_string()))?;
        if s.is_empty() {
            return Err((
                ERR_INVALID_ARG,
                "source paths must not be empty".to_string(),
            ));
        }
        sources.push(PathBuf::from(s));
    }
    let get_path = |key: &str| -> FoResult<Option<PathBuf>> {
        match v.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(val) => match val.as_str() {
                Some(s) if !s.is_empty() => Ok(Some(PathBuf::from(s))),
                _ => Err((
                    ERR_INVALID_ARG,
                    format!("\"{key}\" must be a non-empty string"),
                )),
            },
        }
    };
    let dest_dir = get_path("dest_dir")?;
    let dest = get_path("dest")?;
    let conflict = match v.get("conflict").and_then(Value::as_str) {
        None => Conflict::Fail,
        Some("fail") => Conflict::Fail,
        Some("skip") => Conflict::Skip,
        Some("overwrite") => Conflict::Overwrite,
        Some("rename") => Conflict::Rename,
        Some(other) => {
            return Err((
                ERR_INVALID_ARG,
                format!("unknown conflict policy \"{other}\""),
            ))
        }
    };
    let get_u64 = |key: &str, max: i64| -> FoResult<u64> {
        match v.get(key) {
            None | Some(Value::Null) => Ok(0),
            Some(val) => match val.as_i64() {
                Some(n) if (0..=max).contains(&n) => Ok(n as u64),
                _ => Err((
                    ERR_INVALID_ARG,
                    format!("\"{key}\" must be an integer 0..={max}"),
                )),
            },
        }
    };
    let retries = get_u64("retries", 100)? as u32;
    let retry_delay_ms = get_u64("retry_delay_ms", 60_000)?;
    match op {
        Op::Copy | Op::Move => {
            if dest_dir.is_none() {
                return Err((
                    ERR_INVALID_ARG,
                    format!("{} requires \"dest_dir\"", op.name()),
                ));
            }
        }
        Op::Rename => {
            if sources.len() != 1 {
                return Err((
                    ERR_INVALID_ARG,
                    "rename requires exactly one source".to_string(),
                ));
            }
            if dest.is_none() {
                return Err((ERR_INVALID_ARG, "rename requires \"dest\"".to_string()));
            }
        }
        Op::Delete => {}
    }
    Ok(Spec {
        op,
        sources,
        dest_dir,
        dest,
        conflict,
        retries,
        retry_delay_ms,
        undo_dir: get_path("undo_dir")?,
    })
}

#[derive(Clone, Debug)]
pub struct PlanItem {
    pub source: PathBuf,
    /// Destination path; `None` for delete.
    pub dest: Option<PathBuf>,
    pub bytes: u64,
    pub is_dir: bool,
    pub conflict: bool,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub spec: Spec,
    pub items: Vec<PlanItem>,
    pub total_bytes: u64,
    /// Directories found while expanding directory sources, in
    /// discovery order (parents before children).
    pub source_dirs: Vec<PathBuf>,
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn collect_dir(
    dir: &Path,
    items: &mut Vec<(PathBuf, PathBuf)>,
    dirs: &mut Vec<PathBuf>,
) -> FoResult<()> {
    dirs.push(dir.to_path_buf());
    let mut entries: Vec<PathBuf> = Vec::new();
    let rd = fs::read_dir(dir).map_err(|e| {
        (
            ERR_IO,
            format!("cannot read directory {}: {e}", path_str(dir)),
        )
    })?;
    for ent in rd {
        let ent = ent.map_err(|e| (ERR_IO, format!("cannot read directory entry: {e}")))?;
        entries.push(ent.path());
    }
    entries.sort();
    for path in entries {
        let md = fs::symlink_metadata(&path).map_err(|e| {
            (
                ERR_NOT_FOUND,
                format!("cannot stat {}: {e}", path_str(&path)),
            )
        })?;
        if md.is_dir() && !md.file_type().is_symlink() {
            collect_dir(&path, items, dirs)?;
        } else {
            items.push((path.clone(), path));
        }
    }
    Ok(())
}

/// Expand a spec into a concrete plan: every source file, its
/// destination, its size, and whether that destination already
/// exists (a pre-detected conflict).
pub fn build_plan(spec: Spec) -> FoResult<Plan> {
    let mut items: Vec<PlanItem> = Vec::new();
    let mut source_dirs: Vec<PathBuf> = Vec::new();
    match spec.op {
        Op::Rename => {
            let src = &spec.sources[0];
            let md = fs::symlink_metadata(src).map_err(|e| {
                (
                    ERR_NOT_FOUND,
                    format!("source not found: {} ({e})", path_str(src)),
                )
            })?;
            let dest = spec.dest.clone().unwrap();
            let is_dir = md.is_dir() && !md.file_type().is_symlink();
            let bytes = if is_dir {
                0
            } else {
                fs::metadata(src).map(|m| m.len()).unwrap_or(md.len())
            };
            items.push(PlanItem {
                conflict: dest.exists(),
                source: src.clone(),
                dest: Some(dest),
                bytes,
                is_dir,
            });
        }
        Op::Delete => {
            for src in &spec.sources {
                let md = fs::symlink_metadata(src).map_err(|e| {
                    (
                        ERR_NOT_FOUND,
                        format!("source not found: {} ({e})", path_str(src)),
                    )
                })?;
                if md.is_dir() && !md.file_type().is_symlink() {
                    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
                    collect_dir(src, &mut found, &mut source_dirs)?;
                    for (p, _) in found {
                        let b = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                        items.push(PlanItem {
                            source: p,
                            dest: None,
                            bytes: b,
                            is_dir: false,
                            conflict: false,
                        });
                    }
                } else {
                    items.push(PlanItem {
                        bytes: fs::metadata(src).map(|m| m.len()).unwrap_or(md.len()),
                        source: src.clone(),
                        dest: None,
                        is_dir: false,
                        conflict: false,
                    });
                }
            }
        }
        Op::Copy | Op::Move => {
            let dest_dir = spec.dest_dir.clone().unwrap();
            for src in &spec.sources {
                let md = fs::symlink_metadata(src).map_err(|e| {
                    (
                        ERR_NOT_FOUND,
                        format!("source not found: {} ({e})", path_str(src)),
                    )
                })?;
                let base_name = src.file_name().ok_or((
                    ERR_INVALID_ARG,
                    format!("source has no file name: {}", path_str(src)),
                ))?;
                if md.is_dir() && !md.file_type().is_symlink() {
                    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
                    collect_dir(src, &mut found, &mut source_dirs)?;
                    for (p, _) in found {
                        let rel = p.strip_prefix(src).unwrap().to_path_buf();
                        let dest = dest_dir.join(base_name).join(rel);
                        let b = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                        items.push(PlanItem {
                            conflict: dest.exists(),
                            source: p,
                            dest: Some(dest),
                            bytes: b,
                            is_dir: false,
                        });
                    }
                } else {
                    let dest = dest_dir.join(base_name);
                    items.push(PlanItem {
                        conflict: dest.exists(),
                        bytes: fs::metadata(src).map(|m| m.len()).unwrap_or(md.len()),
                        source: src.clone(),
                        dest: Some(dest),
                        is_dir: false,
                    });
                }
            }
        }
    }
    let total_bytes = items.iter().map(|i| i.bytes).sum();
    Ok(Plan {
        spec,
        items,
        total_bytes,
        source_dirs,
    })
}

pub fn plan_json(plan: &Plan) -> String {
    let items: Vec<Value> = plan
        .items
        .iter()
        .map(|i| {
            json::obj(vec![
                ("source", json::s(&path_str(&i.source))),
                (
                    "dest",
                    match &i.dest {
                        Some(d) => json::s(&path_str(d)),
                        None => Value::Null,
                    },
                ),
                ("bytes", json::n(i.bytes)),
                ("is_dir", json::b(i.is_dir)),
                ("conflict", json::b(i.conflict)),
            ])
        })
        .collect();
    json::obj(vec![
        ("op", json::s(plan.spec.op.name())),
        ("conflict_policy", json::s(plan.spec.conflict.name())),
        ("item_count", json::n(plan.items.len() as u64)),
        ("total_bytes", json::n(plan.total_bytes)),
        (
            "conflict_count",
            json::n(plan.items.iter().filter(|i| i.conflict).count() as u64),
        ),
        ("items", Value::Array(items)),
    ])
    .to_json()
}

/// `fo_plan` core: parse a spec and return the plan JSON.
pub fn plan(spec_json: &str) -> FoResult<String> {
    let spec = parse_spec(spec_json)?;
    let plan = build_plan(spec)?;
    Ok(plan_json(&plan))
}

/// Compute the "name (N).ext" unique variant of `dest`, starting
/// at N=2. A leading dot (dotfiles) is not an extension separator.
pub fn unique_dest(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let parent = dest.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.clone(), String::new()),
    };
    for n in 2..10_000u32 {
        let candidate = parent.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem} (10000){ext}"))
}

/// Run `f` until it succeeds, retrying up to `retries` extra times
/// with `delay` between attempts. Returns the value and the number
/// of attempts made (1 = first try), or the last error and the
/// attempt count.
pub fn with_retries<T, F: FnMut() -> Result<T, String>>(
    retries: u32,
    delay: Duration,
    mut f: F,
) -> (Result<T, String>, u32) {
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        match f() {
            Ok(v) => return (Ok(v), attempts),
            Err(e) => {
                if attempts > retries {
                    return (Err(e), attempts);
                }
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            }
        }
    }
}

fn mtime_of(p: &Path) -> (i64, u32) {
    if let Ok(md) = fs::metadata(p) {
        if let Ok(t) = md.modified() {
            if let Ok(d) = t.duration_since(UNIX_EPOCH) {
                return (d.as_secs() as i64, d.subsec_nanos());
            }
        }
    }
    (0, 0)
}

/// One progress event. `phase` is "item_start", "chunk" or
/// "item_done". Returning `false` from the callback requests
/// cancellation; the engine stops promptly.
#[derive(Clone, Debug)]
pub struct ProgressEvent {
    pub phase: &'static str,
    pub item_index: usize,
    pub item_count: usize,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub item_bytes_done: u64,
    pub item_bytes: u64,
    pub current_path: String,
    pub dest_path: String,
}

impl ProgressEvent {
    pub fn to_json(&self) -> String {
        json::obj(vec![
            ("phase", json::s(self.phase)),
            ("item_index", json::n(self.item_index as u64)),
            ("item_count", json::n(self.item_count as u64)),
            ("bytes_done", json::n(self.bytes_done)),
            ("total_bytes", json::n(self.total_bytes)),
            ("item_bytes_done", json::n(self.item_bytes_done)),
            ("item_bytes", json::n(self.item_bytes)),
            ("current_path", json::s(&self.current_path)),
            ("dest_path", json::s(&self.dest_path)),
        ])
        .to_json()
    }
}

pub type ProgressFn<'a> = dyn FnMut(&ProgressEvent) -> bool + 'a;

#[derive(Clone, Debug)]
struct ItemOutcome {
    source: String,
    dest: Option<String>,
    status: String, // done | skipped | failed | cancelled
    bytes: u64,
    attempts: u32,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct UndoRecord {
    action: String, // created | overwritten | moved | moved_overwrite | deleted
    source: Option<String>,
    dest: Option<String>,
    staged: Option<String>,
    size: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
}

fn io_err(ctx: &str, e: &std::io::Error) -> String {
    format!("{ctx}: {e}")
}

/// Copy `src` to `tmp` in 64 KiB chunks, reporting chunk progress.
/// Returns bytes copied, or Err((message, cancelled)).
#[allow(clippy::too_many_arguments)]
fn copy_chunked(
    src: &Path,
    tmp: &Path,
    item_index: usize,
    item_count: usize,
    base_done: u64,
    total_bytes: u64,
    item_bytes: u64,
    progress: &mut Option<&mut ProgressFn<'_>>,
) -> Result<u64, (String, bool)> {
    let mut input =
        fs::File::open(src).map_err(|e| (io_err(&format!("open {}", path_str(src)), &e), false))?;
    let mut output = fs::File::create(tmp)
        .map_err(|e| (io_err(&format!("create {}", path_str(tmp)), &e), false))?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut done: u64 = 0;
    loop {
        let n = input
            .read(&mut buf)
            .map_err(|e| (io_err("read", &e), false))?;
        if n == 0 {
            break;
        }
        output
            .write_all(&buf[..n])
            .map_err(|e| (io_err("write", &e), false))?;
        done += n as u64;
        if let Some(cb) = progress.as_deref_mut() {
            let ev = ProgressEvent {
                phase: "chunk",
                item_index,
                item_count,
                bytes_done: base_done + done,
                total_bytes,
                item_bytes_done: done,
                item_bytes,
                current_path: path_str(src),
                dest_path: path_str(tmp),
            };
            if !cb(&ev) {
                drop(output);
                let _ = fs::remove_file(tmp);
                return Err(("cancelled by progress callback".to_string(), true));
            }
        }
    }
    output.flush().map_err(|e| (io_err("flush", &e), false))?;
    Ok(done)
}

fn new_undo_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), now)
}

/// `fo_run` core. Job-level failures (bad spec, missing source,
/// conflict policy "fail" with conflicts) are errors; per-item IO
/// failures are recorded in the result JSON, not raised.
#[allow(clippy::too_many_arguments)]
pub fn run(spec_json: &str, mut progress: Option<&mut ProgressFn<'_>>) -> FoResult<String> {
    let spec = parse_spec(spec_json)?;
    let plan = build_plan(spec)?;
    let op = plan.spec.op;
    let conflict = plan.spec.conflict;

    // Preflight for conflict=fail: touch nothing at all.
    if conflict == Conflict::Fail && op != Op::Delete {
        let conflicts: Vec<&PlanItem> = plan.items.iter().filter(|i| i.conflict).collect();
        if !conflicts.is_empty() {
            let items: Vec<Value> = plan
                .items
                .iter()
                .map(|i| {
                    json::obj(vec![
                        ("source", json::s(&path_str(&i.source))),
                        (
                            "dest",
                            match &i.dest {
                                Some(d) => json::s(&path_str(d)),
                                None => Value::Null,
                            },
                        ),
                        (
                            "status",
                            json::s(if i.conflict { "failed" } else { "skipped" }),
                        ),
                        ("bytes", json::n(0)),
                        ("attempts", json::n(0)),
                        (
                            "error",
                            if i.conflict {
                                json::s("destination already exists (conflict policy \"fail\")")
                            } else {
                                Value::Null
                            },
                        ),
                    ])
                })
                .collect();
            return Ok(json::obj(vec![
                ("op", json::s(op.name())),
                ("state", json::s("failed")),
                ("item_count", json::n(plan.items.len() as u64)),
                ("total_bytes", json::n(plan.total_bytes)),
                ("bytes_done", json::n(0)),
                ("items", Value::Array(items)),
                ("undo_id", Value::Null),
                ("undo_manifest", Value::Null),
            ])
            .to_json());
        }
    }

    let undo_id = new_undo_id();
    let undo_root = plan
        .spec
        .undo_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("fileopskit-undo-{undo_id}")));
    let staging_dir = undo_root.join(format!("staging-{undo_id}"));
    let manifest_path = undo_root.join(format!("manifest-{undo_id}.json"));

    let mut outcomes: Vec<ItemOutcome> = Vec::new();
    let mut undo_records: Vec<UndoRecord> = Vec::new();
    let mut bytes_done: u64 = 0;
    let mut cancelled = false;
    let delay = Duration::from_millis(plan.spec.retry_delay_ms);
    let item_count = plan.items.len();

    for (idx, item) in plan.items.iter().enumerate() {
        if cancelled {
            outcomes.push(ItemOutcome {
                source: path_str(&item.source),
                dest: item.dest.as_ref().map(|d| path_str(d)),
                status: "cancelled".to_string(),
                bytes: 0,
                attempts: 0,
                error: None,
            });
            continue;
        }
        // Effective destination after conflict handling.
        let mut eff_dest: Option<PathBuf> = item.dest.clone();
        let mut will_overwrite = false;
        if item.conflict && op != Op::Delete {
            match conflict {
                Conflict::Skip => {
                    outcomes.push(ItemOutcome {
                        source: path_str(&item.source),
                        dest: item.dest.as_ref().map(|d| path_str(d)),
                        status: "skipped".to_string(),
                        bytes: 0,
                        attempts: 0,
                        error: Some("destination already exists (skipped)".to_string()),
                    });
                    continue;
                }
                Conflict::Rename => {
                    if let Some(d) = &item.dest {
                        eff_dest = Some(unique_dest(d));
                    }
                }
                Conflict::Overwrite => will_overwrite = true,
                Conflict::Fail => unreachable!("preflight handled"),
            }
        }

        if let Some(cb) = progress.as_deref_mut() {
            let ev = ProgressEvent {
                phase: "item_start",
                item_index: idx,
                item_count,
                bytes_done,
                total_bytes: plan.total_bytes,
                item_bytes_done: 0,
                item_bytes: item.bytes,
                current_path: path_str(&item.source),
                dest_path: eff_dest.as_ref().map(|d| path_str(d)).unwrap_or_default(),
            };
            if !cb(&ev) {
                cancelled = true;
                outcomes.push(ItemOutcome {
                    source: path_str(&item.source),
                    dest: eff_dest.as_ref().map(|d| path_str(d)),
                    status: "cancelled".to_string(),
                    bytes: 0,
                    attempts: 0,
                    error: None,
                });
                continue;
            }
        }

        let staged_path = staging_dir.join(idx.to_string());
        let item_ref = item;
        let eff_ref = &eff_dest;
        let mut cancel_flag = false;
        let (result, attempts) = with_retries(plan.spec.retries, delay, || {
            run_one(
                op,
                item_ref,
                eff_ref.as_ref(),
                will_overwrite,
                &staged_path,
                idx,
                item_count,
                bytes_done,
                plan.total_bytes,
                &mut progress,
                &mut cancel_flag,
            )
        });
        if cancel_flag {
            cancelled = true;
            outcomes.push(ItemOutcome {
                source: path_str(&item.source),
                dest: eff_dest.as_ref().map(|d| path_str(d)),
                status: "cancelled".to_string(),
                bytes: 0,
                attempts,
                error: None,
            });
            continue;
        }
        match result {
            Ok(record) => {
                bytes_done += item.bytes;
                if let Some(r) = record {
                    undo_records.push(r);
                }
                outcomes.push(ItemOutcome {
                    source: path_str(&item.source),
                    dest: eff_dest.as_ref().map(|d| path_str(d)),
                    status: "done".to_string(),
                    bytes: item.bytes,
                    attempts,
                    error: None,
                });
                if let Some(cb) = progress.as_deref_mut() {
                    let ev = ProgressEvent {
                        phase: "item_done",
                        item_index: idx,
                        item_count,
                        bytes_done,
                        total_bytes: plan.total_bytes,
                        item_bytes_done: item.bytes,
                        item_bytes: item.bytes,
                        current_path: path_str(&item.source),
                        dest_path: eff_dest.as_ref().map(|d| path_str(d)).unwrap_or_default(),
                    };
                    if !cb(&ev) {
                        cancelled = true;
                    }
                }
            }
            Err(msg) => {
                outcomes.push(ItemOutcome {
                    source: path_str(&item.source),
                    dest: eff_dest.as_ref().map(|d| path_str(d)),
                    status: "failed".to_string(),
                    bytes: 0,
                    attempts,
                    error: Some(msg),
                });
            }
        }
    }

    // Clean up emptied source directories for move / delete.
    if (op == Op::Move || op == Op::Delete) && !cancelled {
        let mut dirs = plan.source_dirs.clone();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs {
            let _ = fs::remove_dir(&d); // only succeeds when empty
        }
    }

    // Write the undo manifest if anything changed.
    let (undo_id_json, manifest_json) = if undo_records.is_empty() {
        (Value::Null, Value::Null)
    } else {
        let recs: Vec<Value> = undo_records
            .iter()
            .map(|r| {
                json::obj(vec![
                    ("action", json::s(&r.action)),
                    (
                        "source",
                        match &r.source {
                            Some(s) => json::s(s),
                            None => Value::Null,
                        },
                    ),
                    (
                        "dest",
                        match &r.dest {
                            Some(s) => json::s(s),
                            None => Value::Null,
                        },
                    ),
                    (
                        "staged",
                        match &r.staged {
                            Some(s) => json::s(s),
                            None => Value::Null,
                        },
                    ),
                    ("size", json::n(r.size)),
                    ("mtime_secs", Value::I64(r.mtime_secs)),
                    ("mtime_nanos", json::n(r.mtime_nanos as u64)),
                ])
            })
            .collect();
        let manifest = json::obj(vec![
            ("version", json::n(1)),
            ("undo_id", json::s(&undo_id)),
            ("op", json::s(op.name())),
            ("items", Value::Array(recs)),
        ])
        .to_json();
        let written =
            fs::create_dir_all(&undo_root).and_then(|_| fs::write(&manifest_path, manifest));
        match written {
            Ok(()) => (json::s(&undo_id), json::s(&path_str(&manifest_path))),
            Err(_) => (Value::Null, Value::Null),
        }
    };

    let any_failed = outcomes.iter().any(|o| o.status == "failed");
    let state = if cancelled {
        "cancelled"
    } else if any_failed {
        "completed_with_errors"
    } else {
        "completed"
    };
    let items: Vec<Value> = outcomes
        .iter()
        .map(|o| {
            json::obj(vec![
                ("source", json::s(&o.source)),
                (
                    "dest",
                    match &o.dest {
                        Some(d) => json::s(d),
                        None => Value::Null,
                    },
                ),
                ("status", json::s(&o.status)),
                ("bytes", json::n(o.bytes)),
                ("attempts", json::n(o.attempts as u64)),
                (
                    "error",
                    match &o.error {
                        Some(e) => json::s(e),
                        None => Value::Null,
                    },
                ),
            ])
        })
        .collect();
    Ok(json::obj(vec![
        ("op", json::s(op.name())),
        ("state", json::s(state)),
        ("item_count", json::n(item_count as u64)),
        ("total_bytes", json::n(plan.total_bytes)),
        ("bytes_done", json::n(bytes_done)),
        ("items", Value::Array(items)),
        ("undo_id", undo_id_json),
        ("undo_manifest", manifest_json),
    ])
    .to_json())
}

/// Execute one item. On success returns the undo record to store
/// (None when the operation changed nothing reversible, which
/// does not happen for done items today) — errors are strings so
/// `with_retries` can re-run the whole item.
#[allow(clippy::too_many_arguments)]
fn run_one(
    op: Op,
    item: &PlanItem,
    eff_dest: Option<&PathBuf>,
    will_overwrite: bool,
    staged_path: &Path,
    idx: usize,
    item_count: usize,
    base_done: u64,
    total_bytes: u64,
    progress: &mut Option<&mut ProgressFn<'_>>,
    cancel_flag: &mut bool,
) -> Result<Option<UndoRecord>, String> {
    match op {
        Op::Delete => {
            // Stage a copy, then remove the original.
            if let Some(parent) = staged_path.parent() {
                fs::create_dir_all(parent).map_err(|e| io_err("create staging dir", &e))?;
            }
            fs::copy(&item.source, staged_path).map_err(|e| io_err("stage for delete", &e))?;
            let staged_size = fs::metadata(staged_path).map(|m| m.len()).unwrap_or(0);
            if staged_size != item.bytes {
                let _ = fs::remove_file(staged_path);
                return Err("staged copy size mismatch; original left in place".to_string());
            }
            fs::remove_file(&item.source).map_err(|e| io_err("delete file", &e))?;
            Ok(Some(UndoRecord {
                action: "deleted".to_string(),
                source: Some(path_str(&item.source)),
                dest: None,
                staged: Some(path_str(staged_path)),
                size: item.bytes,
                mtime_secs: 0,
                mtime_nanos: 0,
            }))
        }
        Op::Rename | Op::Move if item.is_dir => {
            let dest = eff_dest.ok_or("missing destination")?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| io_err("create dest dir", &e))?;
            }
            // Stage the overwritten destination directory by rename
            // when one is in the way (same-volume staging only in
            // practice; a copy fallback is not attempted for dirs).
            let mut staged: Option<String> = None;
            if will_overwrite && dest.exists() {
                if let Some(parent) = staged_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| io_err("create staging dir", &e))?;
                }
                fs::rename(dest, staged_path).map_err(|e| io_err("stage overwritten dir", &e))?;
                staged = Some(path_str(staged_path));
            }
            match fs::rename(&item.source, dest) {
                Ok(()) => Ok(Some(UndoRecord {
                    action: if staged.is_some() {
                        "moved_overwrite"
                    } else {
                        "moved"
                    }
                    .to_string(),
                    source: Some(path_str(&item.source)),
                    dest: Some(path_str(dest)),
                    staged,
                    size: 0,
                    mtime_secs: 0,
                    mtime_nanos: 0,
                })),
                Err(e) => {
                    // Best-effort: put the staged directory back.
                    if let Some(st) = &staged {
                        let _ = fs::rename(st, dest);
                    }
                    Err(io_err("rename directory", &e))
                }
            }
        }
        Op::Copy | Op::Move | Op::Rename => {
            let dest = eff_dest.ok_or("missing destination")?;
            if let Some(parent) = dest.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent).map_err(|e| io_err("create dest dir", &e))?;
                }
            }
            // Fast path for move/rename: a plain rename is atomic
            // and preserves everything. Cancellation cannot be
            // observed mid-rename (it does not block on IO chunks).
            if op != Op::Copy && !will_overwrite {
                match fs::rename(&item.source, dest) {
                    Ok(()) => {
                        return Ok(Some(UndoRecord {
                            action: "moved".to_string(),
                            source: Some(path_str(&item.source)),
                            dest: Some(path_str(dest)),
                            staged: None,
                            size: item.bytes,
                            mtime_secs: 0,
                            mtime_nanos: 0,
                        }));
                    }
                    Err(_) => {
                        // Fall through to copy + delete-source
                        // (cross-volume move).
                    }
                }
            }
            // Chunked copy into a partial file next to the dest;
            // the real destination is only touched once the copy
            // is complete, so cancellation never leaves a torn
            // destination behind.
            let partial = dest.with_file_name(format!(
                ".{}.fo-partial",
                dest.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".to_string())
            ));
            let copied = match copy_chunked(
                &item.source,
                &partial,
                idx,
                item_count,
                base_done,
                total_bytes,
                item.bytes,
                progress,
            ) {
                Ok(n) => n,
                Err((msg, was_cancel)) => {
                    if was_cancel {
                        *cancel_flag = true;
                    }
                    return Err(msg);
                }
            };
            let mut staged: Option<String> = None;
            if will_overwrite && dest.exists() {
                if let Some(parent) = staged_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| io_err("create staging dir", &e))?;
                }
                fs::copy(dest, staged_path).map_err(|e| io_err("stage overwritten file", &e))?;
                staged = Some(path_str(staged_path));
            }
            if dest.exists() {
                fs::remove_file(dest).map_err(|e| io_err("replace destination", &e))?;
            }
            fs::rename(&partial, dest).map_err(|e| {
                let _ = fs::remove_file(&partial);
                io_err("finish destination", &e)
            })?;
            let (ms, mn) = mtime_of(dest);
            if op == Op::Copy {
                Ok(Some(UndoRecord {
                    action: if staged.is_some() {
                        "overwritten"
                    } else {
                        "created"
                    }
                    .to_string(),
                    source: None,
                    dest: Some(path_str(dest)),
                    staged,
                    size: copied,
                    mtime_secs: ms,
                    mtime_nanos: mn,
                }))
            } else {
                // Move / rename across volumes: the copy landed;
                // only now remove the source.
                fs::remove_file(&item.source)
                    .map_err(|e| io_err("remove source after copy", &e))?;
                Ok(Some(UndoRecord {
                    action: if staged.is_some() {
                        "moved_overwrite"
                    } else {
                        "moved"
                    }
                    .to_string(),
                    source: Some(path_str(&item.source)),
                    dest: Some(path_str(dest)),
                    staged,
                    size: copied,
                    mtime_secs: ms,
                    mtime_nanos: mn,
                }))
            }
        }
    }
}

/// `fo_undo` core: read a manifest written by `run` and reverse
/// its items in reverse order.
pub fn undo(manifest_path: &str) -> FoResult<String> {
    let text = fs::read_to_string(manifest_path).map_err(|e| {
        (
            ERR_NOT_FOUND,
            format!("cannot read undo manifest {manifest_path}: {e}"),
        )
    })?;
    let v =
        json::parse(&text).map_err(|e| (ERR_INVALID_ARG, format!("invalid undo manifest: {e}")))?;
    let items = v.get("items").and_then(Value::as_array).ok_or((
        ERR_INVALID_ARG,
        "undo manifest has no \"items\" array".to_string(),
    ))?;
    let undo_id = v
        .get("undo_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut results: Vec<Value> = Vec::new();
    let mut any_failed = false;
    for rec in items.iter().rev() {
        let action = rec.get("action").and_then(Value::as_str).unwrap_or("");
        let source = rec.get("source").and_then(Value::as_str).map(PathBuf::from);
        let dest = rec.get("dest").and_then(Value::as_str).map(PathBuf::from);
        let staged = rec.get("staged").and_then(Value::as_str).map(PathBuf::from);
        let size = rec.get("size").and_then(Value::as_i64).unwrap_or(0) as u64;
        let ms = rec.get("mtime_secs").and_then(Value::as_i64).unwrap_or(0);
        let mn = rec.get("mtime_nanos").and_then(Value::as_i64).unwrap_or(0) as u32;
        let (status, error) = undo_one(
            action,
            source.as_deref(),
            dest.as_deref(),
            staged.as_deref(),
            size,
            ms,
            mn,
        );
        if status == "failed" || status == "skipped_modified" || status == "skipped_conflict" {
            any_failed = true;
        }
        results.push(json::obj(vec![
            ("action", json::s(action)),
            (
                "source",
                match &source {
                    Some(p) => json::s(&path_str(p)),
                    None => Value::Null,
                },
            ),
            (
                "dest",
                match &dest {
                    Some(p) => json::s(&path_str(p)),
                    None => Value::Null,
                },
            ),
            ("status", json::s(status)),
            (
                "error",
                match error {
                    Some(e) => json::s(&e),
                    None => Value::Null,
                },
            ),
        ]));
    }
    Ok(json::obj(vec![
        ("undo_id", json::s(&undo_id)),
        (
            "state",
            json::s(if any_failed {
                "completed_with_errors"
            } else {
                "completed"
            }),
        ),
        ("items", Value::Array(results)),
    ])
    .to_json())
}

fn dest_modified(dest: &Path, size: u64, ms: i64, mn: u32) -> bool {
    match fs::metadata(dest) {
        Ok(md) => {
            if md.len() != size {
                return true;
            }
            let (cms, cmn) = mtime_of(dest);
            cms != ms || cmn != mn
        }
        Err(_) => false,
    }
}

fn restore_staged(staged: &Path, target: &Path) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| io_err("create restore dir", &e))?;
        }
    }
    match fs::rename(staged, target) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(staged, target).map_err(|e| io_err("restore staged file", &e))?;
            let _ = fs::remove_file(staged);
            Ok(())
        }
    }
}

fn undo_one(
    action: &str,
    source: Option<&Path>,
    dest: Option<&Path>,
    staged: Option<&Path>,
    size: u64,
    ms: i64,
    mn: u32,
) -> (&'static str, Option<String>) {
    match action {
        "created" => {
            let dest = match dest {
                Some(d) => d,
                None => return ("failed", Some("record has no dest".to_string())),
            };
            if !dest.exists() {
                return ("done", None); // already gone; nothing to do
            }
            if dest_modified(dest, size, ms, mn) {
                return (
                    "skipped_modified",
                    Some("destination changed after the run; left in place".to_string()),
                );
            }
            match fs::remove_file(dest) {
                Ok(()) => ("done", None),
                Err(e) => ("failed", Some(io_err("remove created file", &e))),
            }
        }
        "overwritten" => {
            let (dest, staged) = match (dest, staged) {
                (Some(d), Some(s)) => (d, s),
                _ => return ("failed", Some("record missing dest/staged".to_string())),
            };
            if dest.exists() && (ms != 0 || mn != 0) && dest_modified(dest, size, ms, mn) {
                return (
                    "skipped_modified",
                    Some("destination changed after the run; not restored".to_string()),
                );
            }
            if dest.exists() {
                if let Err(e) = fs::remove_file(dest) {
                    return ("failed", Some(io_err("remove overwritten file", &e)));
                }
            }
            match restore_staged(staged, dest) {
                Ok(()) => ("done", None),
                Err(e) => ("failed", Some(e)),
            }
        }
        "moved" | "moved_overwrite" => {
            let (source, dest) = match (source, dest) {
                (Some(s), Some(d)) => (s, d),
                _ => return ("failed", Some("record missing source/dest".to_string())),
            };
            if source.exists() {
                return (
                    "skipped_conflict",
                    Some("original location is occupied".to_string()),
                );
            }
            if !dest.exists() {
                return ("failed", Some("moved file no longer exists".to_string()));
            }
            if let Some(parent) = source.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = fs::create_dir_all(parent);
                }
            }
            let moved_back = match fs::rename(dest, source) {
                Ok(()) => Ok(()),
                Err(_) => match fs::copy(dest, source) {
                    Ok(_) => {
                        let _ = fs::remove_file(dest);
                        Ok(())
                    }
                    Err(e) => Err(io_err("move file back", &e)),
                },
            };
            if let Err(e) = moved_back {
                return ("failed", Some(e));
            }
            if action == "moved_overwrite" {
                if let Some(st) = staged {
                    if dest.exists() {
                        return (
                            "failed",
                            Some("cannot restore overwritten file: dest occupied".to_string()),
                        );
                    }
                    if let Err(e) = restore_staged(st, dest) {
                        return ("failed", Some(e));
                    }
                }
            }
            ("done", None)
        }
        "deleted" => {
            let (source, staged) = match (source, staged) {
                (Some(s), Some(st)) => (s, st),
                _ => return ("failed", Some("record missing source/staged".to_string())),
            };
            if source.exists() {
                return (
                    "skipped_conflict",
                    Some("original location is occupied".to_string()),
                );
            }
            match restore_staged(staged, source) {
                Ok(()) => ("done", None),
                Err(e) => ("failed", Some(e)),
            }
        }
        _ => ("failed", Some(format!("unknown undo action \"{action}\""))),
    }
}

/// Escape a string for inclusion inside JSON double quotes.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests;
