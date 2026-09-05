use serde_json::{Value, json};
use sned::core::agent_loop::TaskState;
use sned::core::file_editor::AnchorStateManager;
use sned::core::tools::handlers::{edit_file::EditFileHandler, read_file::ReadFileHandler};
use sned::core::tools::{
    ToolContext, ToolError, ToolFailureClass, ToolHandler, ToolRequiredNextStep,
};
use std::sync::Arc;
use tokio::sync::Mutex;

struct Workflow {
    dir: tempfile::TempDir,
    ctx: ToolContext,
}

impl Workflow {
    fn new(bytes: &[u8]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fixture.txt"), bytes).unwrap();
        let ctx = ToolContext::new(
            Arc::new(Mutex::new(TaskState::default())),
            None,
            dir.path().to_path_buf(),
            AnchorStateManager::with_cache_file(dir.path().join("anchors.json")),
            false,
            format!("workflow-{}", dir.path().display()),
            None,
            true,
            Arc::new(sned::cli::output::StderrOutputWriter),
        );
        Self { dir, ctx }
    }

    async fn read_path(&self, path: &str, range: Option<(usize, usize)>) -> Vec<String> {
        let mut params = json!({"paths": [path]});
        if let Some((start, end)) = range {
            params["start_line"] = json!(start);
            params["end_line"] = json!(end);
        }
        let output = ToolHandler::execute(&ReadFileHandler::new(), &self.ctx, params)
            .await
            .unwrap();
        // Preserve source whitespace and annotations exactly as delivered to the model.
        output
            .as_str()
            .unwrap()
            .split('\n')
            .filter(|line| {
                line.split_once('§').is_some_and(|(word, _)| {
                    !word.is_empty() && word.chars().all(char::is_alphanumeric)
                })
            })
            .map(str::to_owned)
            .collect()
    }

    async fn read(&self, range: Option<(usize, usize)>) -> Vec<String> {
        self.read_path("fixture.txt", range).await
    }

    async fn edit(&self, edits: Value) -> Result<Value, ToolError> {
        EditFileHandler::new()
            .execute(
                &self.ctx,
                json!({"files": [{"path": "fixture.txt", "edits": edits}]}),
            )
            .await
    }

    fn assert_bytes(&self, bytes: &[u8]) {
        assert_eq!(
            std::fs::read(self.dir.path().join("fixture.txt")).unwrap(),
            bytes
        );
    }

    async fn assert_reread(&self, expected: bool) {
        let path = self.dir.path().join("fixture.txt").canonicalize().unwrap();
        assert_eq!(
            self.ctx
                .state
                .lock()
                .await
                .must_reread_before_edit
                .contains(path.to_str().unwrap()),
            expected
        );
    }
}

#[tokio::test]
async fn native_workflow_verbatim_whitespace_encoding_matrix() {
    for (before, after) in [
        ("first\n\tvalue  \nlast\n", "first\n\treplaced  \nlast\n"),
        (
            "first\r\n\tvalue  \r\nlast\r\n",
            "first\r\n\treplaced  \r\nlast\r\n",
        ),
        (
            "\u{feff}first\r\n\tvalue  \r\nlast",
            "\u{feff}first\r\n\treplaced  \r\nlast",
        ),
        ("λ\n\tvalue  \n雪", "λ\n\treplaced  \n雪"),
        ("first\n   \nlast", "first\n\treplaced  \nlast"),
        ("first\n\nlast", "first\n\treplaced  \nlast"),
    ] {
        let w = Workflow::new(before.as_bytes());
        let anchors = w.read(None).await;
        w.edit(json!([{"anchor": anchors[1], "text": "\treplaced  "}]))
            .await
            .unwrap();
        w.assert_bytes(after.as_bytes());
        w.assert_reread(false).await;
    }
}

#[tokio::test]
async fn native_workflow_repeated_ranges_and_successive_reuse() {
    let w = Workflow::new(b"alpha\nbeta\ngamma\ndelta\n");
    let full = w.read(None).await;
    assert_eq!(w.read(Some((2, 3))).await, full[1..3]);
    assert_eq!(w.read(None).await, full);
    assert_eq!(w.read(Some((3, 4))).await, full[2..4]);
    assert_eq!(w.read(Some((3, 4))).await, full[2..4]);
    for (i, replacement) in ["A", "B", "C", "D"].into_iter().enumerate() {
        w.edit(json!([{"anchor": full[i], "text": replacement}]))
            .await
            .unwrap();
        w.assert_reread(false).await;
    }
    w.assert_bytes(b"A\nB\nC\nD\n");
    let next = w.read(None).await;
    w.edit(json!([{"anchor": next[2], "text": "final"}]))
        .await
        .unwrap();
    w.assert_bytes(b"A\nB\nfinal\nD\n");
}

#[tokio::test]
async fn native_workflow_duplicate_occurrences_outside_range() {
    let block = "void same(void)\n{\n/* same */\n\n}\n";
    for index in 0..5 {
        let before = format!("{block}{block}");
        let w = Workflow::new(before.as_bytes());
        let full = w.read(None).await;
        let selected = w.read(Some((index + 6, index + 6))).await;
        assert_eq!(selected[0], full[index + 5]);
        assert!(selected[0].contains(&format!("lines {}", index + 1)));
        w.edit(json!([{"anchor": selected[0], "text": "changed"}]))
            .await
            .unwrap();
        let mut expected: Vec<&str> = before.split('\n').collect();
        expected[index + 5] = "changed";
        w.assert_bytes(expected.join("\n").as_bytes());
    }
}

#[tokio::test]
async fn native_workflow_atomic_rejection_reread_and_retry() {
    let w = Workflow::new(b"alpha\nbeta\n");
    let anchors = w.read(None).await;
    w.edit(json!([{"anchor": anchors[0], "text": "A"}]))
        .await
        .unwrap();
    let error = w
        .edit(json!([
            {"anchor": anchors[1], "text": "B"},
            {"anchor": anchors[0], "text": "bad"}
        ]))
        .await
        .unwrap_err();
    let metadata = error.metadata().unwrap();
    assert_eq!(metadata.class, ToolFailureClass::AnchorInvalid);
    assert_eq!(
        metadata.required_next_step,
        Some(ToolRequiredNextStep::ReadFile)
    );
    assert!(error.to_string().contains("read_file"));
    w.assert_bytes(b"A\nbeta\n");
    w.assert_reread(true).await;
    let fresh = w.read(None).await;
    w.assert_reread(false).await;
    w.edit(json!([{"anchor": fresh[0], "text": "final"}, {"anchor": fresh[1], "text": "B"}]))
        .await
        .unwrap();
    w.assert_bytes(b"final\nB\n");
}

#[tokio::test]
async fn native_workflow_insertion_replay_has_no_reread_latch() {
    let w = Workflow::new(b"before\nanchor\nafter\n");
    let anchor = w.read(None).await[1].clone();
    let edits = json!([{"anchor": anchor, "edit_type": "insert_before", "text": "\ninserted\n"}]);
    w.edit(edits.clone()).await.unwrap();
    w.assert_bytes(b"before\n\ninserted\n\nanchor\nafter\n");
    let error = w.edit(edits).await.unwrap_err();
    assert!(error.to_string().to_lowercase().contains("duplicate"));
    w.assert_reread(false).await;
    w.assert_bytes(b"before\n\ninserted\n\nanchor\nafter\n");
}

#[tokio::test]
async fn native_workflow_external_change_and_path_alias_recovery() {
    let w = Workflow::new(b"alpha\nbeta\n");
    let anchor = w.read(None).await[0].clone();
    let modified = std::fs::metadata(w.dir.path().join("fixture.txt"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::write(w.dir.path().join("fixture.txt"), b"external\nbeta\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(w.dir.path().join("fixture.txt"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert!(
        w.edit(json!([{"anchor": anchor, "text": "unsafe"}]))
            .await
            .is_err()
    );
    w.assert_bytes(b"external\nbeta\n");
    w.assert_reread(true).await;
    let canonical = w.dir.path().join("fixture.txt").canonicalize().unwrap();
    let fresh = w.read_path(canonical.to_str().unwrap(), None).await;
    w.assert_reread(false).await;
    assert_eq!(fresh, w.read_path("./fixture.txt", None).await);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("fixture.txt", w.dir.path().join("alias.txt")).unwrap();
        w.ctx
            .state
            .lock()
            .await
            .must_reread_before_edit
            .insert(canonical.to_string_lossy().into_owned());
        assert_eq!(fresh, w.read_path("alias.txt", None).await);
        w.assert_reread(false).await;
    }
    w.edit(json!([{"anchor": fresh[0], "text": "safe"}]))
        .await
        .unwrap();
    w.assert_bytes(b"safe\nbeta\n");
}

#[tokio::test]
async fn native_workflow_c_guards_preserve_escapes() {
    let before = "void int3_exception_notify(void) {}\nvoid int3_selftest(void) { puts(\"a\\n\\\\b\"); }\nvoid flush_ptrace_hw_breakpoint(void) {}\n";
    let w = Workflow::new(before.as_bytes());
    let a = w.read(None).await;
    w.edit(json!([{"anchor": a[0], "end_anchor": a[1], "text": "#ifdef CONFIG_KPROBES\nvoid int3_exception_notify(void) {}\nvoid int3_selftest(void) { puts(\"a\\n\\\\b\"); }\n#endif"}])).await.unwrap();
    w.edit(json!([{"anchor": a[2], "text": "#ifdef CONFIG_HAVE_HW_BREAKPOINT\nvoid flush_ptrace_hw_breakpoint(void) {}\n#endif"}])).await.unwrap();
    w.assert_bytes(b"#ifdef CONFIG_KPROBES\nvoid int3_exception_notify(void) {}\nvoid int3_selftest(void) { puts(\"a\\n\\\\b\"); }\n#endif\n#ifdef CONFIG_HAVE_HW_BREAKPOINT\nvoid flush_ptrace_hw_breakpoint(void) {}\n#endif\n");
    if let Ok(output) = std::process::Command::new("cc")
        .args([
            "-x",
            "c",
            "-fsyntax-only",
            "-include",
            "stdio.h",
            "-DCONFIG_KPROBES",
            "-DCONFIG_HAVE_HW_BREAKPOINT",
        ])
        .arg(w.dir.path().join("fixture.txt"))
        .output()
    {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn native_workflow_oversized_read_is_explicitly_inspection_only() {
    let w = Workflow::new("x\n".repeat(300_000).as_bytes());
    let output = ToolHandler::execute(
        &ReadFileHandler::new(),
        &w.ctx,
        json!({"paths": ["fixture.txt"], "start_line": 1, "end_line": 2}),
    )
    .await
    .unwrap();
    assert!(output.as_str().unwrap().contains("inspection only"));
    let anchors = w.read(Some((1, 2))).await;
    let error = w
        .edit(json!([{"anchor": anchors[0], "text": "no"}]))
        .await
        .unwrap_err();
    assert_eq!(
        error.metadata().unwrap().required_next_step,
        Some(ToolRequiredNextStep::AskUser)
    );
    w.assert_bytes("x\n".repeat(300_000).as_bytes());
}

#[tokio::test]
async fn native_workflow_restart_preserves_occurrence_history() {
    let mut w = Workflow::new(b"same\nsame\ntail\n");
    let a = w.read(None).await;
    let cache = w.dir.path().join("anchors.json");
    let persisted: Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    assert!(persisted.get(&w.ctx.task_id).is_some());
    w.ctx.anchor_mgr = AnchorStateManager::with_cache_file(cache.clone());
    assert_eq!(w.read(None).await, a);
    w.edit(json!([{"anchor": a[0], "text": "different"}]))
        .await
        .unwrap();
    let after = w.read(None).await;
    w.ctx.anchor_mgr = AnchorStateManager::with_cache_file(cache);
    w.ctx.state = Arc::new(Mutex::new(TaskState::default()));
    assert_eq!(w.read(None).await, after);
    w.edit(json!([{"anchor": after[1], "text": "second"}]))
        .await
        .unwrap();
    w.assert_reread(false).await;
    w.assert_bytes(b"different\nsecond\ntail\n");
}

#[tokio::test]
async fn native_workflow_schema_error_does_not_force_reread() {
    let w = Workflow::new(b"alpha missing anchor is stale\nbeta\n");
    let a = w.read(None).await;
    let error = w
        .edit(json!([{"anchor": a[0], "edit_type": "replace", "content": ["alpha"], "text": "A"}]))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("'content' field"));
    assert!(!error.to_string().contains("unknown or stale"), "{error}");
    assert!(
        !error.to_string().contains("Re-read the target file"),
        "{error}"
    );
    w.assert_bytes(b"alpha missing anchor is stale\nbeta\n");
    w.assert_reread(false).await;
    w.edit(json!([{"anchor": a[0], "text": "A"}]))
        .await
        .unwrap();
    w.assert_bytes(b"A\nbeta\n");
}

#[cfg(unix)]
#[tokio::test]
async fn native_workflow_symlinked_workspace_accepts_canonical_path() {
    let mut w = Workflow::new(b"alpha\nbeta\n");
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("workspace");
    std::os::unix::fs::symlink(w.dir.path(), &alias).unwrap();
    w.ctx.workspace_root = alias;
    let original = w.read(None).await;
    let canonical = w.dir.path().join("fixture.txt").canonicalize().unwrap();
    w.ctx
        .state
        .lock()
        .await
        .must_reread_before_edit
        .insert(canonical.to_string_lossy().into_owned());
    let reread = w.read_path(canonical.to_str().unwrap(), None).await;
    assert_eq!(reread, original);
    w.assert_reread(false).await;
    w.edit(json!([{"anchor": reread[0], "text": "A"}]))
        .await
        .unwrap();
    w.assert_bytes(b"A\nbeta\n");
}

#[tokio::test]
async fn native_workflow_per_file_atomicity_keeps_independent_success() {
    let w = Workflow::new(b"old\n");
    std::fs::write(w.dir.path().join("other.txt"), b"other\n").unwrap();
    let stale = w.read(None).await[0].clone();
    let other = w.read_path("other.txt", None).await[0].clone();
    w.edit(json!([{"anchor": stale, "text": "current"}]))
        .await
        .unwrap();
    let error = ToolHandler::execute(
        &EditFileHandler::new(),
        &w.ctx,
        json!({"files": [
            {"path": "fixture.txt", "edits": [{"anchor": stale, "text": "unsafe"}]},
            {"path": "other.txt", "edits": [{"anchor": other, "text": "safe"}]}
        ]}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("1 edit(s) applied"));
    w.assert_bytes(b"current\n");
    assert_eq!(
        std::fs::read(w.dir.path().join("other.txt")).unwrap(),
        b"safe\n"
    );
    w.assert_reread(true).await;
    assert_eq!(error.metadata().unwrap().affected_paths.len(), 1);
}

#[tokio::test]
async fn native_workflow_large_snapshot_never_retargets_duplicate() {
    let mut before = String::from("head\nsame\nsame\n");
    for i in 3..5002 {
        before.push_str(&format!("unique_{i}\n"));
    }
    let w = Workflow::new(before.as_bytes());
    let a = w.read(Some((1, 3))).await;
    assert_eq!(a, w.read(Some((1, 3))).await);
    let guidance = ToolHandler::execute(
        &ReadFileHandler::new(),
        &w.ctx,
        json!({"paths": ["fixture.txt"], "start_line": 1, "end_line": 3}),
    )
    .await
    .unwrap();
    assert!(
        guidance
            .as_str()
            .unwrap()
            .contains("old anchors cannot be reused")
    );
    w.edit(json!([{"anchor": a[0], "edit_type": "insert_before", "text": "inserted"}]))
        .await
        .unwrap();
    let after_insert = format!("inserted\n{before}");
    // Positional L3 now names the other identical occurrence after insertion.
    let result = w.edit(json!([{"anchor": a[2], "text": "selected"}])).await;
    assert!(
        result.is_err(),
        "a stale large-file anchor must not edit a different occurrence"
    );
    w.assert_bytes(after_insert.as_bytes());
    w.assert_reread(true).await;
    let fresh = w.read(Some((4, 4))).await;
    w.edit(json!([{"anchor": fresh[0], "text": "selected"}]))
        .await
        .unwrap();
    let expected = after_insert.replacen("head\nsame\nsame\n", "head\nsame\nselected\n", 1);
    w.assert_bytes(expected.as_bytes());
}

#[tokio::test]
async fn native_workflow_copied_replacement_does_not_write_display_annotations() {
    let w = Workflow::new(b"/* duplicate */\nunchanged\n/* duplicate */\n");
    let a = w.read(None).await;
    assert!(a[2].contains("[identical content also at lines 1]"));
    let copied_replacement = a[2].replace("/* duplicate */", "/* selected */");
    w.edit(json!([{"anchor": a[2], "text": copied_replacement}]))
        .await
        .unwrap();
    w.assert_bytes(b"/* duplicate */\nunchanged\n/* selected */\n");
    w.assert_reread(false).await;
    let fresh = w.read(None).await;
    w.edit(json!([{"anchor": fresh[1], "text": "// literal [identical content also at lines 1]"}]))
        .await
        .unwrap();
    w.assert_bytes(
        b"/* duplicate */\n// literal [identical content also at lines 1]\n/* selected */\n",
    );
}

#[tokio::test]
async fn native_workflow_ranged_read_bounds_duplicate_metadata() {
    let before = "same\n".repeat(100_000);
    let w = Workflow::new(before.as_bytes());
    let selected = w.read(Some((50_000, 50_000))).await;

    assert_eq!(selected.len(), 1);
    assert!(selected[0].contains("identical content also at lines"));
    assert!(selected[0].contains("(99991 more)"));
}
