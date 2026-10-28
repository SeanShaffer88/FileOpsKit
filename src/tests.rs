use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "fileopskit-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
    fn write(&self, rel: &str, data: &[u8]) -> PathBuf {
        let p = self.p(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, data).unwrap();
        p
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn spec_json(v: Value) -> String {
    v.to_json()
}
fn run_ok(spec: &str) -> Value {
    let out = run(spec, None).unwrap();
    json::parse(&out).unwrap()
}
fn item_statuses(v: &Value) -> Vec<String> {
    v.get("items")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i.get("status").unwrap().as_str().unwrap().to_string())
        .collect()
}

#[test]
fn plan_expands_tree_and_detects_conflicts() {
    let t = Tmp::new("plan");
    t.write("src/a.txt", b"hello");
    t.write("src/sub/b.txt", b"world!");
    t.write("out/src/a.txt", b"old");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("src")))]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
    ]));
    let pv = json::parse(&plan(&spec).unwrap()).unwrap();
    assert_eq!(pv.get("item_count").unwrap().as_i64(), Some(2));
    assert_eq!(pv.get("total_bytes").unwrap().as_i64(), Some(11));
    assert_eq!(pv.get("conflict_count").unwrap().as_i64(), Some(1));
}

#[test]
fn copy_tree_preserves_contents() {
    let t = Tmp::new("copy");
    t.write("src/a.txt", b"hello");
    t.write("src/sub/b.bin", &[7u8; 1000]);
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("src")))]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(v.get("state").unwrap().as_str(), Some("completed"));
    assert_eq!(fs::read(t.p("out/src/a.txt")).unwrap(), b"hello");
    assert_eq!(fs::read(t.p("out/src/sub/b.bin")).unwrap().len(), 1000);
    assert!(t.p("src/a.txt").exists(), "copy keeps the source");
    assert!(v.get("undo_manifest").unwrap().as_str().is_some());
}

#[test]
fn move_removes_source_after_success() {
    let t = Tmp::new("move");
    t.write("src/a.txt", b"data");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("move")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("src/a.txt")))]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(v.get("state").unwrap().as_str(), Some("completed"));
    assert!(!t.p("src/a.txt").exists());
    assert_eq!(fs::read(t.p("out/a.txt")).unwrap(), b"data");
    // Undo moves it back.
    let manifest = v
        .get("undo_manifest")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let uv = json::parse(&undo(&manifest).unwrap()).unwrap();
    assert_eq!(uv.get("state").unwrap().as_str(), Some("completed"));
    assert_eq!(fs::read(t.p("src/a.txt")).unwrap(), b"data");
    assert!(!t.p("out/a.txt").exists());
}

#[test]
fn rename_single_file() {
    let t = Tmp::new("rename");
    t.write("old.txt", b"x");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("rename")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("old.txt")))]),
        ),
        ("dest", json::s(&path_str(&t.p("new name.txt")))),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(v.get("state").unwrap().as_str(), Some("completed"));
    assert!(!t.p("old.txt").exists());
    assert_eq!(fs::read(t.p("new name.txt")).unwrap(), b"x");
}

#[test]
fn delete_then_undo_restores() {
    let t = Tmp::new("delundo");
    t.write("victim.txt", b"precious");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("delete")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("victim.txt")))]),
        ),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(v.get("state").unwrap().as_str(), Some("completed"));
    assert!(!t.p("victim.txt").exists());
    let manifest = v
        .get("undo_manifest")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let uv = json::parse(&undo(&manifest).unwrap()).unwrap();
    assert_eq!(uv.get("state").unwrap().as_str(), Some("completed"));
    assert_eq!(fs::read(t.p("victim.txt")).unwrap(), b"precious");
}

#[test]
fn overwrite_stages_original_and_undo_restores_it() {
    let t = Tmp::new("overwrite");
    t.write("new.txt", b"new-content");
    t.write("out/new.txt", b"original-content");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("new.txt")))]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("conflict", json::s("overwrite")),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(v.get("state").unwrap().as_str(), Some("completed"));
    assert_eq!(fs::read(t.p("out/new.txt")).unwrap(), b"new-content");
    let manifest = v
        .get("undo_manifest")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let uv = json::parse(&undo(&manifest).unwrap()).unwrap();
    assert_eq!(uv.get("state").unwrap().as_str(), Some("completed"));
    assert_eq!(fs::read(t.p("out/new.txt")).unwrap(), b"original-content");
}

#[test]
fn rename_conflict_picks_unique_name() {
    let t = Tmp::new("renconf");
    t.write("a.txt", b"incoming");
    t.write("out/a.txt", b"existing");
    t.write("out/a (2).txt", b"also existing");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("a.txt")))]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("conflict", json::s("rename")),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(v.get("state").unwrap().as_str(), Some("completed"));
    assert_eq!(fs::read(t.p("out/a.txt")).unwrap(), b"existing");
    assert_eq!(fs::read(t.p("out/a (3).txt")).unwrap(), b"incoming");
    let dest = v.get("items").unwrap().as_array().unwrap()[0]
        .get("dest")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        dest.ends_with("a (3).txt"),
        "result lists the renamed dest: {dest}"
    );
}

#[test]
fn skip_policy_leaves_conflicts_alone() {
    let t = Tmp::new("skip");
    t.write("a.txt", b"incoming");
    t.write("b.txt", b"fresh");
    t.write("out/a.txt", b"existing");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![
                json::s(&path_str(&t.p("a.txt"))),
                json::s(&path_str(&t.p("b.txt"))),
            ]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("conflict", json::s("skip")),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(item_statuses(&v), vec!["skipped", "done"]);
    assert_eq!(fs::read(t.p("out/a.txt")).unwrap(), b"existing");
    assert_eq!(fs::read(t.p("out/b.txt")).unwrap(), b"fresh");
}

#[test]
fn fail_policy_preflight_touches_nothing() {
    let t = Tmp::new("failpre");
    t.write("a.txt", b"incoming");
    t.write("b.txt", b"fresh");
    t.write("out/a.txt", b"existing");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![
                json::s(&path_str(&t.p("a.txt"))),
                json::s(&path_str(&t.p("b.txt"))),
            ]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("conflict", json::s("fail")),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(v.get("state").unwrap().as_str(), Some("failed"));
    assert_eq!(fs::read(t.p("out/a.txt")).unwrap(), b"existing");
    assert!(
        !t.p("out/b.txt").exists(),
        "fail preflight must not copy ANY item"
    );
    assert_eq!(v.get("undo_manifest").unwrap(), &Value::Null);
}

#[test]
fn cancel_via_callback_stops_and_reports() {
    let t = Tmp::new("cancel");
    t.write("big.bin", &vec![3u8; 256 * 1024]);
    t.write("later.txt", b"later");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![
                json::s(&path_str(&t.p("big.bin"))),
                json::s(&path_str(&t.p("later.txt"))),
            ]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let mut cb = |ev: &ProgressEvent| -> bool { ev.phase != "chunk" };
    let out = run(&spec, Some(&mut cb)).unwrap();
    let v = json::parse(&out).unwrap();
    assert_eq!(v.get("state").unwrap().as_str(), Some("cancelled"));
    assert_eq!(item_statuses(&v), vec!["cancelled", "cancelled"]);
    assert!(
        !t.p("out/big.bin").exists(),
        "partial copy must not remain as a dest"
    );
    assert!(!t.p("out/later.txt").exists());
}

#[test]
fn retries_are_counted_per_item() {
    let t = Tmp::new("retry");
    t.write("a.txt", b"data");
    t.write("blocker", b"i am a file");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("a.txt")))]),
        ),
        // dest_dir lives *under a file*, so creating it always fails.
        ("dest_dir", json::s(&path_str(&t.p("blocker/sub")))),
        ("retries", Value::I64(2)),
        ("retry_delay_ms", Value::I64(1)),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    assert_eq!(
        v.get("state").unwrap().as_str(),
        Some("completed_with_errors")
    );
    let item = &v.get("items").unwrap().as_array().unwrap()[0];
    assert_eq!(item.get("status").unwrap().as_str(), Some("failed"));
    assert_eq!(item.get("attempts").unwrap().as_i64(), Some(3));
}

#[test]
fn retry_helper_recovers_from_flaky_failures() {
    let mut calls = 0;
    let (res, attempts) = with_retries(5, Duration::ZERO, || {
        calls += 1;
        if calls < 3 {
            Err("flaky".to_string())
        } else {
            Ok(42)
        }
    });
    assert_eq!(res.unwrap(), 42);
    assert_eq!(attempts, 3);
    let (res2, attempts2) = with_retries(2, Duration::ZERO, || -> Result<i32, String> {
        Err("always".to_string())
    });
    assert!(res2.is_err());
    assert_eq!(attempts2, 3);
}

#[test]
fn undo_copy_refuses_to_delete_modified_dest() {
    let t = Tmp::new("modundo");
    t.write("a.txt", b"original-bytes");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("a.txt")))]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    let manifest = v
        .get("undo_manifest")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    // The user edits the copy after the run.
    fs::write(t.p("out/a.txt"), b"user edits, different size").unwrap();
    let uv = json::parse(&undo(&manifest).unwrap()).unwrap();
    assert_eq!(
        uv.get("state").unwrap().as_str(),
        Some("completed_with_errors")
    );
    let item = &uv.get("items").unwrap().as_array().unwrap()[0];
    assert_eq!(
        item.get("status").unwrap().as_str(),
        Some("skipped_modified")
    );
    assert_eq!(
        fs::read(t.p("out/a.txt")).unwrap(),
        b"user edits, different size"
    );
}

#[test]
fn undo_copy_removes_unmodified_dest() {
    let t = Tmp::new("cleanundo");
    t.write("a.txt", b"bytes");
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("a.txt")))]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let v = run_ok(&spec);
    let manifest = v
        .get("undo_manifest")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let uv = json::parse(&undo(&manifest).unwrap()).unwrap();
    assert_eq!(uv.get("state").unwrap().as_str(), Some("completed"));
    assert!(!t.p("out/a.txt").exists());
}

#[test]
fn progress_reports_monotonic_bytes() {
    let t = Tmp::new("progress");
    t.write("big.bin", &vec![9u8; 200 * 1024]);
    let spec = spec_json(json::obj(vec![
        ("op", json::s("copy")),
        (
            "sources",
            Value::Array(vec![json::s(&path_str(&t.p("big.bin")))]),
        ),
        ("dest_dir", json::s(&path_str(&t.p("out")))),
        ("undo_dir", json::s(&path_str(&t.p("undo")))),
    ]));
    let mut seen: Vec<u64> = Vec::new();
    let mut cb = |ev: &ProgressEvent| -> bool {
        seen.push(ev.bytes_done);
        true
    };
    let out = run(&spec, Some(&mut cb)).unwrap();
    let v = json::parse(&out).unwrap();
    assert_eq!(v.get("state").unwrap().as_str(), Some("completed"));
    assert!(
        seen.len() >= 4,
        "start + chunks + done events, got {seen:?}"
    );
    assert!(
        seen.windows(2).all(|w| w[1] >= w[0]),
        "bytes_done never decreases"
    );
    assert_eq!(*seen.last().unwrap(), 200 * 1024);
}

#[test]
fn spec_validation() {
    assert!(parse_spec("{}").is_err());
    assert!(parse_spec("{\"op\":\"copy\",\"sources\":[]}").is_err());
    assert!(parse_spec("{\"op\":\"copy\",\"sources\":[\"x\"]}").is_err()); // no dest_dir
    assert!(parse_spec("{\"op\":\"rename\",\"sources\":[\"a\",\"b\"],\"dest\":\"c\"}").is_err());
    assert!(parse_spec("{\"op\":\"bogus\",\"sources\":[\"x\"]}").is_err());
    assert!(parse_spec("{\"op\":\"delete\",\"sources\":[\"x\"],\"wat\":1}").is_err());
    assert!(parse_spec("{\"op\":\"delete\",\"op\":\"copy\",\"sources\":[\"x\"]}").is_err()); // dup key
    assert!(plan("{\"op\":\"delete\",\"sources\":[\"/definitely/not/here-fo\"]}").is_err());
}

#[test]
fn unique_dest_naming() {
    let t = Tmp::new("unique");
    t.write("f.txt", b"1");
    assert_eq!(unique_dest(&t.p("other.txt")), t.p("other.txt"));
    assert_eq!(unique_dest(&t.p("f.txt")), t.p("f (2).txt"));
    t.write("f (2).txt", b"2");
    assert_eq!(unique_dest(&t.p("f.txt")), t.p("f (3).txt"));
    t.write(".hidden", b"h");
    assert_eq!(unique_dest(&t.p(".hidden")), t.p(".hidden (2)"));
}
