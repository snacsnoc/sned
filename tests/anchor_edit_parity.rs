use sned::core::edit_batch::BatchProcessor;
use sned::core::file_editor::{AnchorStateManager, Edit, EditExecutor, split_content_lines};

fn lines(content: &str) -> Vec<String> {
    split_content_lines(content)
}

fn anchor_lines(absolute_path: &str, content: &str, task_id: &str) -> Vec<String> {
    let content_lines = lines(content);
    let anchor_mgr = AnchorStateManager::new();
    let anchors = anchor_mgr.reconcile(absolute_path, &content_lines, Some(task_id));
    content_lines
        .into_iter()
        .zip(anchors)
        .map(|(line, anchor)| format!("{anchor}§{line}"))
        .collect()
}

#[test]
fn anchor_reconcile_matches_ts_semantics() {
    let task_id = "parity-anchor";
    let path = "/tmp/sned-anchor-parity.txt";
    let anchor_mgr = AnchorStateManager::new();
    anchor_mgr.reset(Some(task_id));

    let initial = lines("line 1\nline 2\nline 3");
    let anchors1 = anchor_mgr.reconcile(path, &initial, Some(task_id));
    assert_eq!(anchors1.len(), 3);
    assert!(
        anchors1
            .iter()
            .all(|anchor| anchor.chars().next().unwrap().is_ascii_uppercase())
    );

    let anchors2 = anchor_mgr.reconcile(path, &initial, Some(task_id));
    assert_eq!(anchors1, anchors2);

    let inserted = lines("line 1\nline 1.5\nline 2\nline 3");
    let anchors3 = anchor_mgr.reconcile(path, &inserted, Some(task_id));
    assert_eq!(anchors3.len(), 4);
    assert_eq!(anchors3[0], anchors1[0]);
    assert_eq!(anchors3[2], anchors1[1]);
    assert_eq!(anchors3[3], anchors1[2]);
    assert_ne!(anchors3[1], anchors1[0]);
    assert_ne!(anchors3[1], anchors1[1]);
    assert_ne!(anchors3[1], anchors1[2]);

    let deleted = lines("line 1\nline 3");
    let anchors4 = anchor_mgr.reconcile(path, &deleted, Some(task_id));
    assert_eq!(anchors4.len(), 2);
    assert_eq!(anchors4[0], anchors3[0]);
    assert_eq!(anchors4[1], anchors3[3]);

    let other_task_anchors = anchor_mgr.reconcile(path, &initial, Some("parity-anchor-2"));
    assert_eq!(anchors4[0], other_task_anchors[0]);
}

#[test]
fn edit_executor_matches_ts_validation_messages() {
    let executor = EditExecutor::new();
    let content = "line 1\nline 2\nline 3";
    let content_lines = lines(content);
    let task_id = "parity-edit";
    let anchor_mgr = AnchorStateManager::new();
    anchor_mgr.reset(Some(task_id));
    let anchored = anchor_lines("/tmp/sned-edit-parity.txt", content, task_id);
    let line_hashes: Vec<String> = anchored
        .iter()
        .map(|line| line.split('§').next().unwrap().to_string())
        .collect();
    let missing_anchor = {
        let mut candidate = "Qwzqzqzq".to_string();
        while line_hashes.contains(&candidate) {
            candidate.push('x');
        }
        candidate
    };

    let valid_edit = Edit {
        anchor: anchored[1].clone(),
        end_anchor: Some(anchored[1].clone()),
        edit_type: "replace".to_string(),
        text: "new line 2".to_string(),
    };
    let bad_format = Edit {
        anchor: "bad§line 1".to_string(),
        end_anchor: Some("bad§line 1".to_string()),
        edit_type: "replace".to_string(),
        text: "new line 1".to_string(),
    };
    let missing = Edit {
        anchor: format!("{missing_anchor}§line 1"),
        end_anchor: Some(format!("{missing_anchor}§line 1")),
        edit_type: "replace".to_string(),
        text: "new line 1".to_string(),
    };
    let wrong_content = Edit {
        anchor: format!("{}§wrong content", anchored[0].split('§').next().unwrap()),
        end_anchor: Some(format!(
            "{}§wrong content",
            anchored[0].split('§').next().unwrap()
        )),
        edit_type: "replace".to_string(),
        text: "wrong content".to_string(),
    };

    let (resolved, failed) = executor.resolve_edits(
        &[
            valid_edit.clone(),
            bad_format.clone(),
            missing.clone(),
            wrong_content.clone(),
        ],
        &content_lines,
        &line_hashes,
    );

    assert_eq!(resolved.len(), 1);
    assert_eq!(failed.len(), 3);
    assert!(failed[0].error.contains("incorrectly formatted"));
    assert!(failed[1].error.contains("not found in the file"));
    assert!(
        failed[2]
            .error
            .contains("does not match the file's content")
    );

    let Some((final_lines, added, removed, applied)) =
        executor.apply_edits(&content_lines, &resolved)
    else {
        panic!("apply_edits returned None (overlapping edits)");
    };
    assert_eq!(added, 1);
    assert_eq!(removed, 1);
    assert_eq!(final_lines, lines("line 1\nnew line 2\nline 3"));
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].original_start_idx, 1);
    assert_eq!(applied[0].lines_added, 1);
    assert_eq!(applied[0].lines_deleted, 1);
}

#[test]
fn file_editor_matches_ts_partial_success_flow() {
    let task_id = "parity-file-editor";
    let path = "/tmp/sned-file-editor-parity.txt";
    let content = "line 1\nline 2\nline 3\nline 4\nline 5";
    let anchor_mgr = AnchorStateManager::new();
    anchor_mgr.reset(Some(task_id));

    let current_hashes = anchor_mgr.reconcile(path, &lines(content), Some(task_id));
    let anchored: Vec<String> = lines(content)
        .into_iter()
        .zip(current_hashes.iter())
        .map(|(line, hash)| format!("{hash}§{line}"))
        .collect();
    let missing_anchor = {
        let mut candidate = "Qwzqzqzq".to_string();
        while anchored
            .iter()
            .any(|line| line.starts_with(&format!("{candidate}§")))
        {
            candidate.push('x');
        }
        candidate
    };
    let edits = vec![
        Edit {
            anchor: anchored[1].clone(),
            end_anchor: Some(anchored[1].clone()),
            edit_type: "replace".to_string(),
            text: "new line 2".to_string(),
        },
        Edit {
            anchor: format!("{missing_anchor}§this should fail"),
            end_anchor: Some(format!("{missing_anchor}§this should fail")),
            edit_type: "replace".to_string(),
            text: "this should fail".to_string(),
        },
        Edit {
            anchor: anchored[3].clone(),
            end_anchor: Some(anchored[3].clone()),
            edit_type: "replace".to_string(),
            text: "new line 4".to_string(),
        },
    ];

    let processor = BatchProcessor::new(sned::core::edit_batch::DiffMode::Full);
    let prepared = processor
        .prepare_edits(
            path,
            "sned-file-editor-parity.txt",
            content,
            &edits,
            &anchor_mgr.reconcile(path, &lines(content), Some(task_id)),
        )
        .unwrap();
    let mut prepared = prepared;
    let batch = processor.apply_batch(
        &mut prepared,
        "sned-file-editor-parity.txt",
        "sned-file-editor-parity.txt",
    );

    assert!(batch.success);
    assert!(prepared.diff.contains("<<<<<<< SEARCH"));
    assert!(prepared.diff.contains(">>>>>>> REPLACE"));
    assert!(
        batch
            .final_content
            .as_deref()
            .unwrap()
            .contains("new line 2")
    );
    assert!(
        batch
            .final_content
            .as_deref()
            .unwrap()
            .contains("new line 4")
    );
}

#[test]
fn file_editor_preserves_trailing_newline_semantics() {
    let task_id = "parity-trailing-newline";
    let path = "/tmp/sned-trailing-newline-parity.txt";
    let content = "line 1\nline 2\n";
    let anchor_mgr = AnchorStateManager::new();
    anchor_mgr.reset(Some(task_id));

    let current_hashes = anchor_mgr.reconcile(path, &lines(content), Some(task_id));
    let anchored: Vec<String> = lines(content)
        .into_iter()
        .zip(current_hashes.iter())
        .map(|(line, hash)| format!("{hash}§{line}"))
        .collect();
    let edits = vec![Edit {
        anchor: anchored[1].clone(),
        end_anchor: Some(anchored[1].clone()),
        edit_type: "replace".to_string(),
        text: "new line 2".to_string(),
    }];

    let processor = BatchProcessor::new(sned::core::edit_batch::DiffMode::Full);
    let prepared = processor
        .prepare_edits(
            path,
            "sned-trailing-newline-parity.txt",
            content,
            &edits,
            &anchor_mgr.reconcile(path, &lines(content), Some(task_id)),
        )
        .unwrap();
    let mut prepared = prepared;
    let batch = processor.apply_batch(
        &mut prepared,
        "sned-trailing-newline-parity.txt",
        "sned-trailing-newline-parity.txt",
    );

    assert!(batch.success);
    assert_eq!(batch.final_content.as_deref(), Some("line 1\nnew line 2\n"));
    assert_eq!(prepared.final_lines, lines("line 1\nnew line 2\n"));
}
