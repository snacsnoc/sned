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

#[tokio::test]
async fn native_workflow_diff_context_exposes_usable_duplicate_anchors() {
    let w = Workflow::new(b"head\n}\ntarget\n}\ntail\n");
    let a = w.read(None).await;
    let result = w
        .edit(json!([{"anchor":a[2],"text":"changed"}]))
        .await
        .unwrap();
    let ansi = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    let output = ansi.replace_all(result.as_str().unwrap(), "");
    let context = output
        .lines()
        .find_map(|line| line.strip_prefix(' ').filter(|line| line.ends_with("§}")))
        .expect("diff must include the preceding brace");
    w.edit(json!([{"anchor":context,"text":"} // first"}]))
        .await
        .unwrap();
    w.assert_bytes(b"head\n} // first\nchanged\n}\ntail\n");
}

#[tokio::test]
async fn native_workflow_reference_search_locks_requested_and_indexed_files_together() {
    use sned::core::tools::handlers::find_symbol_references::FindSymbolReferencesHandler;
    use sned::services::symbol_index::{SymbolIndexService, SymbolLocation, SymbolType};
    let w = Workflow::new(b"");
    let other = Workflow::new(b"");
    let mut second = other.ctx.clone();
    second.workspace_root = w.dir.path().to_path_buf();
    let mut index = SymbolIndexService::new(
        w.dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    for path in ["a.rs", "b.rs"] {
        std::fs::write(w.dir.path().join(path), "fn foo() {}\n").unwrap();
        index.index_file(
            path,
            1,
            12,
            &[SymbolLocation {
                path: None,
                name: "foo".into(),
                start_line: 0,
                start_column: 3,
                end_line: 0,
                end_column: 6,
                symbol_type: SymbolType::Definition,
                kind: None,
            }],
        );
    }
    let index = Arc::new(std::sync::Mutex::new(index));
    let one = FindSymbolReferencesHandler::new().with_symbol_index(index.clone());
    let two = FindSymbolReferencesHandler::new().with_symbol_index(index);
    let results = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            ToolHandler::execute(&one, &w.ctx, json!({"paths":["b.rs","b.rs"],"name":"foo"})),
            ToolHandler::execute(&two, &second, json!({"paths":["a.rs"],"name":"foo"}))
        )
    })
    .await
    .expect("reference searches must not deadlock on duplicate or inverted paths");
    for result in [results.0, results.1] {
        let output = result.unwrap();
        assert!(output.as_str().unwrap().contains("a.rs:"));
        assert!(output.as_str().unwrap().contains("b.rs:"));
    }
}

#[tokio::test]
async fn native_workflow_structural_reads_keep_read_file_anchors() {
    use sned::core::tools::handlers::{
        find_symbol_references::FindSymbolReferencesHandler, get_function::GetFunctionHandler,
    };
    for newline in ["\n", "\r\n"] {
        let w = Workflow::new(b"");
        let source = ["fn foo() {", "}", "", "fn main() {", "    foo();", "}", ""].join(newline);
        std::fs::write(w.dir.path().join("code.rs"), &source).unwrap();
        let a = w.read_path("code.rs", None).await;
        ToolHandler::execute(
            &GetFunctionHandler,
            &w.ctx,
            json!({"path":"code.rs", "name":"foo"}),
        )
        .await
        .unwrap();
        assert_eq!(a, w.read_path("code.rs", None).await);
        ToolHandler::execute(
            &FindSymbolReferencesHandler::new(),
            &w.ctx,
            json!({"paths":["code.rs"], "name":"foo"}),
        )
        .await
        .unwrap();
        assert_eq!(a, w.read_path("code.rs", None).await);
        EditFileHandler::new().execute(&w.ctx,json!({"files":[{"path":"code.rs","edits":[{"anchor":a[4],"text":"    foo(); // checked"}]}]})).await.unwrap();
        let expected = [
            "fn foo() {",
            "}",
            "",
            "fn main() {",
            "    foo(); // checked",
            "}",
            "",
        ]
        .join(newline);
        assert_eq!(
            std::fs::read(w.dir.path().join("code.rs")).unwrap(),
            expected.as_bytes()
        );
    }
}

#[tokio::test]
async fn native_workflow_symbol_replace_then_native_edit() {
    use sned::core::tools::handlers::replace_symbol::ReplaceSymbolHandler;
    let w = Workflow::new(b"");
    std::fs::write(
        w.dir.path().join("code.rs"),
        "const NAME: &str = \"雪\";\r\nfn foo() {}\r\nfn other() {}\r\n",
    )
    .unwrap();
    w.read_path("code.rs", None).await;
    ToolHandler::execute(&ReplaceSymbolHandler::new(), &w.ctx, json!({"replacements":[{"path":"code.rs","symbol":"foo","text":"fn foo() { /* fixed */ }"}]})).await.unwrap();
    assert_eq!(
        std::fs::read(w.dir.path().join("code.rs")).unwrap(),
        "const NAME: &str = \"雪\";\r\nfn foo() { /* fixed */ }\r\nfn other() {}\r\n".as_bytes()
    );
    let a = w.read_path("code.rs", None).await;
    EditFileHandler::new().execute(&w.ctx,json!({"files":[{"path":"code.rs","edits":[{"anchor":a[2],"text":"fn other() { /* checked */ }"}]}]})).await.unwrap();
    assert_eq!(
        std::fs::read(w.dir.path().join("code.rs")).unwrap(),
        "const NAME: &str = \"雪\";\r\nfn foo() { /* fixed */ }\r\nfn other() { /* checked */ }\r\n".as_bytes()
    );
}

#[tokio::test]
async fn native_workflow_indexed_symbol_replace_uses_crlf_byte_offsets() {
    use sned::core::tools::handlers::replace_symbol::ReplaceSymbolHandler;
    use sned::services::symbol_index::{SymbolIndexService, SymbolLocation, SymbolType};
    let w = Workflow::new(b"");
    let source = "const NAME: &str = \"雪\";\r\nfn foo() {}\r\nfn other() {}\r\n";
    std::fs::write(w.dir.path().join("code.rs"), source).unwrap();
    w.read_path("code.rs", None).await;
    let mut index = SymbolIndexService::new(
        w.dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    index.index_file(
        "code.rs",
        1,
        source.len() as u64,
        &[SymbolLocation {
            path: None,
            name: "foo".into(),
            start_line: 1,
            start_column: 0,
            end_line: 1,
            end_column: 11,
            symbol_type: SymbolType::Definition,
            kind: Some("function".into()),
        }],
    );
    let handler =
        ReplaceSymbolHandler::new().with_symbol_index(Arc::new(std::sync::Mutex::new(index)));
    ToolHandler::execute(&handler, &w.ctx, json!({"replacements":[{"path":"code.rs","symbol":"foo","text":"fn foo() { /* fixed */ }"}]})).await.unwrap();
    assert_eq!(
        std::fs::read(w.dir.path().join("code.rs")).unwrap(),
        "const NAME: &str = \"雪\";\r\nfn foo() { /* fixed */ }\r\nfn other() {}\r\n".as_bytes()
    );
}

#[tokio::test]
async fn native_workflow_locks_span_independent_contexts_and_helpers() {
    let first = Workflow::new(b"source\n");
    let second = Workflow::new(b"other\n");
    let path = first.dir.path().join("fixture.txt").canonicalize().unwrap();
    let held = first.ctx.lock_file_paths(std::slice::from_ref(&path)).await;
    assert!(sned::core::file_editor::FileEditGuard::try_acquire(path.to_str().unwrap()).is_none());
    let paths = [path.clone()];
    let pending = second.ctx.lock_file_paths(&paths);
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    drop(held);
    let second_held = pending.await;
    assert!(sned::core::file_editor::FileEditGuard::try_acquire(path.to_str().unwrap()).is_none());
    drop(second_held);
    assert!(sned::core::file_editor::FileEditGuard::try_acquire(path.to_str().unwrap()).is_some());
}

#[tokio::test]
async fn native_workflow_insert_structural_lines_and_replay() {
    for (before, text, after) in [
        (
            "top\n\nbottom",
            "fn a() {}\n\nfn b() {}",
            "top\nfn a() {}\n\nfn b() {}\n\nbottom",
        ),
        (
            "top\n}\nbottom",
            "if (ok) {\n    run();\n}",
            "top\nif (ok) {\n    run();\n}\n}\nbottom",
        ),
        ("top\n);\nbottom", "call(\n);", "top\ncall(\n);\n);\nbottom"),
    ] {
        let w = Workflow::new(before.as_bytes());
        let a = w.read(None).await;
        w.edit(json!([{"anchor": a[1], "edit_type": "insert_before", "text": text}]))
            .await
            .unwrap();
        w.assert_bytes(after.as_bytes());
        let fresh = w.read(None).await;
        let index = 1 + text.split('\n').count();
        let error = w
            .edit(json!([{"anchor": fresh[index], "edit_type": "insert_before", "text": text}]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("duplicate insertion"));
        w.assert_reread(false).await;
        w.assert_bytes(after.as_bytes());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn native_workflow_alias_write_requires_reread_then_recovers() {
    use sned::core::tools::handlers::write_to_file::WriteToFileHandler;

    let w = Workflow::new(b"first\nsecond\n");
    std::os::unix::fs::symlink("fixture.txt", w.dir.path().join("alias.txt")).unwrap();
    let original = w.read_path("alias.txt", None).await;

    ToolHandler::execute(
        &WriteToFileHandler::new(),
        &w.ctx,
        json!({"path": "alias.txt", "content": "first\nreplacement\n"}),
    )
    .await
    .unwrap();

    let error = w
        .edit_path(
            "alias.txt",
            json!([{"anchor": original[1], "text": "unsafe"}]),
        )
        .await
        .expect_err("a whole-file write must block stale alias anchors");
    assert_eq!(
        error.metadata().unwrap().required_next_step,
        Some(ToolRequiredNextStep::ReadFile)
    );
    w.assert_bytes(b"first\nreplacement\n");

    let fresh = w.read_path("alias.txt", None).await;
    let output = w
        .edit_path("alias.txt", json!([{"anchor": fresh[1], "text": "final"}]))
        .await
        .unwrap();
    assert!(
        !output.as_str().unwrap().contains("not read this session"),
        "alias read must satisfy the edit-session check: {output:?}"
    );
    w.assert_bytes(b"first\nfinal\n");
}

#[tokio::test]
async fn native_workflow_insertion_may_contain_a_guard_anchor_in_its_body() {
    let w = Workflow::new(b"    return None;\nnext\n");
    let anchors = w.read(None).await;
    w.edit(json!([{
        "anchor": anchors[0],
        "edit_type": "insert_before",
        "text": "    if missing {\n        return None;\n    }"
    }]))
    .await
    .expect("a guard may legitimately contain the anchored return");
    w.assert_bytes(b"    if missing {\n        return None;\n    }\n    return None;\nnext\n");
}

#[tokio::test]
async fn native_workflow_diff_blocks_follow_source_order() {
    let w = Workflow::new(b"first\nsecond\nthird\nfourth\n");
    let anchors = w.read(None).await;
    let output = w
        .edit(json!([
            {"anchor": anchors[3], "text": "fourth changed"},
            {"anchor": anchors[0], "text": "first changed"}
        ]))
        .await
        .unwrap();
    w.assert_bytes(b"first changed\nsecond\nthird\nfourth changed\n");
    let ansi = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    let output = ansi.replace_all(output.as_str().unwrap(), "");
    assert!(
        output.find("first changed").unwrap() < output.find("fourth changed").unwrap(),
        "diff blocks must follow source order: {output}"
    );
}

#[tokio::test]
async fn native_workflow_failed_edit_reread_clears_canonical_freshness_latch() {
    let w = Workflow::new(b"first\nsecond\n");
    let anchors = w.read(None).await;
    let stale = anchors[0].replacen('§', "§wrong ", 1);
    assert!(
        w.edit(json!([{"anchor": stale, "text": "broken"}]))
            .await
            .is_err()
    );
    w.assert_reread(true).await;
    let fresh = w.read(None).await;
    w.assert_reread(false).await;
    w.edit(json!([{"anchor": fresh[1], "text": "second changed"}]))
        .await
        .unwrap();
    w.assert_bytes(b"first\nsecond changed\n");
}

#[tokio::test]
async fn native_workflow_diff_marks_only_changed_lines() {
    for (edit_type, text, expected, additions) in [
        (
            "insert_after",
            "added",
            "before\nanchor\nadded\nafter\n",
            vec!["added"],
        ),
        (
            "insert_after",
            "one\ntwo",
            "before\nanchor\none\ntwo\nafter\n",
            vec!["one", "two"],
        ),
        ("replace", "", "before\nafter\n", vec![]),
    ] {
        let w = Workflow::new(b"before\nanchor\nafter\n");
        let a = w.read(None).await;
        let output = w
            .edit(json!([{"anchor": a[1], "edit_type": edit_type, "text": text}]))
            .await
            .unwrap();
        w.assert_bytes(expected.as_bytes());
        let ansi = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        let output = ansi.replace_all(output.as_str().unwrap(), "");
        let added: Vec<_> = output
            .lines()
            .filter(|line| line.starts_with('+'))
            .filter_map(|line| line.split_once('§').map(|(_, text)| text))
            .collect();
        assert_eq!(added, additions, "{output}");
        let removed_count = usize::from(edit_type == "replace");
        assert!(
            output.contains(&format!("(+{}, -{removed_count} lines)", additions.len())),
            "{output}"
        );
        if edit_type == "insert_after" {
            assert!(!output.contains("lines between"), "{output}");
            assert!(
                output
                    .lines()
                    .any(|line| line.starts_with(' ') && line.ends_with("§anchor")),
                "{output}"
            );
        }
    }
}

#[tokio::test]
async fn native_workflow_deleting_last_line_without_newline_has_valid_diff() {
    let w = Workflow::new(b"first\nlast");
    let a = w.read(None).await;
    let output = w.edit(json!([{"anchor":a[1],"text":""}])).await.unwrap();
    w.assert_bytes(b"first");
    assert!(output.as_str().unwrap().contains("(+0, -1 lines)"));
}

#[tokio::test]
async fn native_workflow_duplicate_identity_cannot_move_across_reads() {
    let w = Workflow::new(b"first\nsame\nsame\nend\n");
    let a = w.read(None).await;
    std::fs::write(w.dir.path().join("fixture.txt"), b"first\nsame\nend\n").unwrap();
    let fresh = w.read(None).await;
    assert!(
        w.edit(json!([{"anchor": a[1], "text": "wrong"}]))
            .await
            .is_err()
    );
    w.assert_bytes(b"first\nsame\nend\n");
    let retry = w.read(None).await;
    assert_eq!(fresh, retry);
    w.edit(json!([{"anchor": retry[1], "text": "right"}]))
        .await
        .unwrap();
    w.assert_bytes(b"first\nright\nend\n");
}

#[tokio::test]
async fn native_workflow_identical_duplicate_reread_keeps_copied_anchor_usable() {
    let w = Workflow::new(b"head\n}\n}\ntail\n");
    let first = w.read(None).await;
    assert_eq!(first, w.read(None).await);
    w.edit(json!([{"anchor": first[1], "text": "} // first"}]))
        .await
        .unwrap();
    w.assert_bytes(b"head\n} // first\n}\ntail\n");
}

#[cfg(unix)]
#[tokio::test]
async fn native_workflow_read_clears_persisted_alias_latches() {
    let w = Workflow::new(b"first\nsecond\n");
    let alias = w.dir.path().join("alias.txt");
    std::os::unix::fs::symlink("fixture.txt", &alias).unwrap();
    {
        let mut state = w.ctx.state.lock().await;
        state
            .must_reread_before_edit
            .insert(alias.to_string_lossy().into_owned());
        state.must_reread_before_edit.insert(
            w.dir
                .path()
                .join("fixture.txt")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
    }
    let a = w.read(None).await;
    assert!(w.ctx.state.lock().await.must_reread_before_edit.is_empty());
    w.edit(json!([{"anchor": a[1], "text": "changed"}]))
        .await
        .unwrap();
    w.assert_bytes(b"first\nchanged\n");
}

#[tokio::test]
async fn native_workflow_whole_write_requires_and_recovers_fresh_anchors() {
    use sned::core::tools::handlers::write_to_file::WriteToFileHandler;
    let w = Workflow::new(b"first\nsecond\n");
    let a = w.read(None).await;
    ToolHandler::execute(
        &WriteToFileHandler::new(),
        &w.ctx,
        json!({"path":"fixture.txt", "content":"first\nreplacement\n"}),
    )
    .await
    .unwrap();
    w.assert_reread(true).await;
    assert!(
        w.edit(json!([{"anchor":a[0], "text":"unsafe"}]))
            .await
            .is_err()
    );
    w.assert_bytes(b"first\nreplacement\n");
    let fresh = w.read(None).await;
    w.assert_reread(false).await;
    w.edit(json!([{"anchor":fresh[1], "text":"final"}]))
        .await
        .unwrap();
    w.assert_bytes(b"first\nfinal\n");
}

#[tokio::test]
async fn native_workflow_symbol_rename_preserves_unicode_crlf_and_occurrences() {
    use sned::core::tools::handlers::rename_symbol::RenameSymbolHandler;
    let w = Workflow::new(b"");
    let before = "fn foo() {}\r\nfn main() { let s = \"雪\"; foo(); foo(); }\r\n";
    std::fs::write(w.dir.path().join("code.rs"), before).unwrap();
    let old = w.read_path("code.rs", None).await;
    ToolHandler::execute(
        &RenameSymbolHandler::new(),
        &w.ctx,
        json!({"paths":["code.rs"],"existing_symbol":"foo","new_symbol":"longer_name"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(w.dir.path().join("code.rs")).unwrap(),
        "fn longer_name() {}\r\nfn main() { let s = \"雪\"; longer_name(); longer_name(); }\r\n"
            .as_bytes()
    );
    assert!(
        EditFileHandler::new()
            .execute(
                &w.ctx,
                json!({"files":[{"path":"code.rs","edits":[{"anchor":old[0],"text":"unsafe"}]}]})
            )
            .await
            .is_err()
    );
    let a = w.read_path("code.rs", None).await;
    EditFileHandler::new().execute(&w.ctx,json!({"files":[{"path":"code.rs","edits":[{"anchor":a[0],"text":"fn longer_name() { /* edited */ }"}]}]})).await.unwrap();
    assert_eq!(std::fs::read(w.dir.path().join("code.rs")).unwrap(), "fn longer_name() { /* edited */ }\r\nfn main() { let s = \"雪\"; longer_name(); longer_name(); }\r\n".as_bytes());
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
        self.edit_path("fixture.txt", edits).await
    }

    async fn edit_path(&self, path: &str, edits: Value) -> Result<Value, ToolError> {
        EditFileHandler::new()
            .execute(
                &self.ctx,
                json!({"files": [{"path": path, "edits": edits}]}),
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
        if !selected[0].ends_with("§") {
            assert!(selected[0].contains(&format!("lines {}", index + 1)));
        }
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
