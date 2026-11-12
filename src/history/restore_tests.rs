use super::*;
use crate::history::tests::{fixture, save as capture_fixture};
use std::sync::atomic::Ordering;
fn journal(store: &Store, request: &str) -> (Connection, String, Action) {
    let account = binding(&store.config, "test").unwrap();
    let id = action_key(&account, request).unwrap();
    let (_, connection) = open(&store.home, true).unwrap().unwrap();
    let (fingerprint, text): (String, String) = connection
        .query_row(
            "SELECT fingerprint,value FROM actions WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let action = serde_json::from_str(&text).unwrap();
    (connection, fingerprint, action)
}
#[tokio::test]
async fn verification_rejects_changed_content_directory_kinds_and_manifests() {
    let (_temp, store, disk) = fixture();
    disk.put("folder/sub/file", b"old");
    let captured = capture_fixture(&store, &disk, "folder", "original").await;
    let account = binding(&store.config, "test").unwrap();
    let (_, connection) = open(&store.home, false).unwrap().unwrap();
    let archive = load(&connection, &captured.id, &account).unwrap();
    let confirmed = archive
        .nodes
        .iter()
        .map(|node| (node.path.clone(), node.entry.clone()))
        .collect::<BTreeMap<_, _>>();
    assert!(
        verify_directory(&disk, "folder/sub/file", &archive, &confirmed)
            .await
            .unwrap_err()
            .to_string()
            .contains("is a file")
    );
    let mut fewer = archive.clone();
    fewer.nodes.pop();
    assert!(verify_directory(&disk, "folder", &fewer, &confirmed)
        .await
        .unwrap_err()
        .to_string()
        .contains("directory changed"));
    let before = disk.stat("folder/sub/file").await.unwrap().unwrap();
    disk.put("folder/sub/file", b"concurrent");
    assert!(same(&disk, "folder/sub/file", &before)
        .await
        .unwrap_err()
        .to_string()
        .contains("target changed"));
    assert!(verify_directory(&disk, "folder", &archive, &confirmed)
        .await
        .unwrap_err()
        .to_string()
        .contains("contents changed"));
}
#[tokio::test]
async fn staging_resumes_known_nodes_and_refuses_occupied_or_changed_ones() {
    let (_temp, store, disk) = fixture();
    disk.put("folder/sub/file", b"old");
    let captured = capture_fixture(&store, &disk, "folder", "original").await;
    let account = binding(&store.config, "test").unwrap();
    let occupied = backend::join(
        "",
        &format!(
            ".rdrive-history-restore-{}",
            action_key(&account, "occupied").unwrap()
        ),
    )
    .unwrap();
    disk.mkdir(&occupied).await.unwrap();
    assert!(restore(
        &store,
        &disk,
        "test",
        &captured.id,
        Some("not-created"),
        None,
        None,
        "occupied"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("staging name is occupied"));
    restore(
        &store,
        &disk,
        "test",
        &captured.id,
        Some("copy"),
        None,
        None,
        "resume",
    )
    .await
    .unwrap();
    let (connection, fingerprint, mut saved) = journal(&store, "resume");
    let stage = saved.staging.as_ref().unwrap().clone();
    let expected = disk.stat("copy").await.unwrap().unwrap();
    disk.rename_checked("copy", &stage, &expected)
        .await
        .unwrap();
    saved.result = None;
    saved.phase = "staging".into();
    save(&connection, &saved, &fingerprint).unwrap();
    drop(connection);
    assert_eq!(
        restore(
            &store,
            &disk,
            "test",
            &captured.id,
            Some("copy"),
            None,
            None,
            "resume"
        )
        .await
        .unwrap()["restored"],
        true
    );
    let (connection, fingerprint, mut saved) = journal(&store, "resume");
    let expected = disk.stat("copy").await.unwrap().unwrap();
    disk.rename_checked("copy", &stage, &expected)
        .await
        .unwrap();
    saved.result = None;
    saved.phase = "staging".into();
    save(&connection, &saved, &fingerprint).unwrap();
    disk.put(&backend::join(&stage, "sub/file").unwrap(), b"changed");
    assert!(restore(
        &store,
        &disk,
        "test",
        &captured.id,
        Some("copy"),
        None,
        None,
        "resume"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("staged history item changed"));
    saved.nodes.remove("sub/file");
    save(&connection, &saved, &fingerprint).unwrap();
    assert!(restore(
        &store,
        &disk,
        "test",
        &captured.id,
        Some("copy"),
        None,
        None,
        "resume"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("descendant is unexpectedly occupied"));
    restore(
        &store,
        &disk,
        "test",
        &captured.id,
        Some("other"),
        None,
        None,
        "ambiguous",
    )
    .await
    .unwrap();
    let (connection, fingerprint, mut saved) = journal(&store, "ambiguous");
    disk.mkdir(saved.staging.as_deref().unwrap()).await.unwrap();
    saved.result = None;
    saved.phase = "moving".into();
    save(&connection, &saved, &fingerprint).unwrap();
    assert!(restore(
        &store,
        &disk,
        "test",
        &captured.id,
        Some("other"),
        None,
        None,
        "ambiguous"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("could not be reconciled"));
    disk.bad_move_reply.store(true, Ordering::SeqCst);
    assert!(restore(
        &store,
        &disk,
        "test",
        &captured.id,
        Some("bad-reply"),
        None,
        None,
        "bad-reply"
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("returned another identity"));
    assert_eq!(
        restore(
            &store,
            &disk,
            "test",
            &captured.id,
            Some("bad-reply"),
            None,
            None,
            "bad-reply"
        )
        .await
        .unwrap()["restored"],
        true
    );
}
#[tokio::test]
async fn recycle_refuses_tree_races_ambiguous_moves_and_invalid_server_receipts() {
    let (_temp, store, disk) = fixture();
    disk.put("folder/file", b"old");
    *disk.fail.lock().unwrap() = Some("rename".into());
    assert!(recycle(&store, &disk, "test", "folder", true, "race")
        .await
        .is_err());
    *disk.fail.lock().unwrap() = None;
    disk.put("folder/file", b"changed");
    assert!(recycle(&store, &disk, "test", "folder", true, "race")
        .await
        .unwrap_err()
        .to_string()
        .contains("contents changed"));
    let (_, _, saved) = journal(&store, "race");
    disk.mkdir(&saved.target).await.unwrap();
    assert!(recycle(&store, &disk, "test", "folder", true, "race")
        .await
        .unwrap_err()
        .to_string()
        .contains("publication is uncertain"));
    disk.put("file", b"old");
    disk.bad_move_reply.store(true, Ordering::SeqCst);
    assert!(recycle(&store, &disk, "test", "file", false, "bad-recycle")
        .await
        .unwrap_err()
        .to_string()
        .contains("returned another item"));
    assert_eq!(
        recycle(&store, &disk, "test", "file", false, "bad-recycle")
            .await
            .unwrap()["recycled"],
        true
    );
}

#[tokio::test]
async fn failed_journal_or_confirmation_writes_keep_all_recovery_payloads() {
    let (_temp, store, disk) = fixture();
    disk.put("file", b"old");
    let captured = capture_fixture(&store, &disk, "file", "original").await;
    assert!(action_key("account", "").is_err());
    assert!(action_key("account", &"x".repeat(4097)).is_err());
    restore(
        &store,
        &disk,
        "test",
        &captured.id,
        Some("copy"),
        None,
        None,
        "copy",
    )
    .await
    .unwrap();
    let (_, fingerprint, saved) = journal(&store, "copy");
    assert!(save(&Connection::open_in_memory().unwrap(), &saved, &fingerprint).is_err());
    disk.put("folder/file", b"tree");
    let directory = capture_fixture(&store, &disk, "folder", "directory").await;
    let (root, connection) = open(&store.home, true).unwrap().unwrap();
    let account = binding(&store.config, "test").unwrap();
    let archive = load(&connection, &directory.id, &account).unwrap();
    let mut bad = Action {
        version: 1,
        id: state::key("bad"),
        account: account.clone(),
        profile: "test".into(),
        entry_id: directory.id.clone(),
        target: "/bad/target".into(),
        expected: None,
        expected_metadata: None,
        phase: "prepared".into(),
        confirmed: None,
        prior: None,
        staging: None,
        nodes: BTreeMap::new(),
        result: None,
    };
    assert!(
        restore_directory(&store, &disk, &root, connection, &archive, &mut bad, "invalid")
            .await
            .is_err()
    );
    *disk.fail_confirmation.lock().unwrap() = Some(store.home.join("history/history.sqlite3"));
    assert!(
        recycle(&store, &disk, "test", "file", false, "confirm-failure")
            .await
            .unwrap_err()
            .to_string()
            .contains("confirmation failure")
    );
    let (connection, _, saved) = journal(&store, "confirm-failure");
    assert!(disk.at(&saved.target).exists());
    assert_eq!(
        lookup(&store.home, &store.config, "test", &saved.entry_id)
            .unwrap()
            .state,
        "prepared"
    );
    connection
        .execute_batch("DROP TRIGGER fail_confirmation")
        .unwrap();
    assert_eq!(
        recycle(&store, &disk, "test", "file", false, "confirm-failure")
            .await
            .unwrap()["recycled"],
        true
    );
}
