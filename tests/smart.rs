use anyhow::Result;
use rdrive::{
    smart::{self, FailureKind, MutationState, Queue},
    state::{self, Store},
};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

fn setup() -> Result<(tempfile::TempDir, PathBuf, Store)> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let store = Store::open(root.join("state"))?;
    Ok((temporary, root, store))
}
fn request(id: &str, item: &str) -> Value {
    json!({"v":1,"op":"modify","domain":"domain","id":item,"request_id":id,
        "base_content_version":"remote-A","base_metadata_version":"metadata-A"})
}
fn edited(root: &Path, name: &str, bytes: &[u8]) -> Result<File> {
    let path = root.join(name);
    fs::write(&path, bytes)?;
    Ok(OpenOptions::new().read(true).write(true).open(path)?)
}
fn bytes(queue: &Queue, id: &str) -> Result<Vec<u8>> {
    let lease = queue.snapshot(id)?.unwrap();
    let mut file = lease.try_clone_file()?;
    file.seek(SeekFrom::Start(0))?;
    let mut value = Vec::new();
    file.read_to_end(&mut value)?;
    Ok(value)
}

#[test]
fn durable_edit_survives_source_deletion_and_unconfirmed_worker_restart() -> Result<()> {
    let (_temporary, root, store) = setup()?;
    let binding = state::key("backend");
    let mut source = edited(&root, "source", b"locally saved important bytes")?;
    source.seek(SeekFrom::Start(5))?;
    let mut queue = Queue::open(&store, "domain", &binding)?;
    let mut input = request("request", "item");
    input["auth_token"] = json!("NEVER_PERSIST_SECRET");
    input["content_path"] = json!(root.join("source"));
    let mutation = queue.enqueue(input, Some(&source))?;
    assert_eq!(
        source.stream_position()?,
        5,
        "snapshot capture must not move caller's descriptor cursor"
    );
    assert_eq!(mutation.state, MutationState::Pending);
    assert!(mutation.response.is_none());
    source.set_len(0)?;
    source.write_all(b"newer unrelated local contents")?;
    drop(source);
    fs::remove_file(root.join("source"))?;
    assert_eq!(
        bytes(&queue, &mutation.id)?,
        b"locally saved important bytes"
    );
    queue.start(&mutation.id)?;
    let read = smart::read_only(&store.home, "domain", &binding)?;
    assert_eq!(
        read[0].state,
        MutationState::Running,
        "UI reads must not interrupt a live worker"
    );
    assert!(
        Queue::open(&store, "domain", &binding).is_err(),
        "another worker must not replay concurrently"
    );
    let journal_path = store
        .home
        .join("smart")
        .join(state::key("domain"))
        .join("queue.json");
    let journal = fs::read_to_string(&journal_path)?;
    assert!(!journal.contains("NEVER_PERSIST_SECRET"));
    assert!(!journal.contains("content_path"));
    drop(queue);
    let mut queue = Queue::open(&store, "domain", &binding)?;
    assert_eq!(queue.get(&mutation.id)?.state, MutationState::Interrupted);
    assert_eq!(
        bytes(&queue, &mutation.id)?,
        b"locally saved important bytes"
    );
    queue.start(&mutation.id)?;
    queue.complete(
        &mutation.id,
        json!({"item":{"id":"item","content_version":"remote-B"}}),
    )?;
    assert_eq!(queue.status()?.dirty_bytes, 0);
    assert!(queue.status()?.completed_snapshot_bytes > 0);
    drop(queue);
    let queue = Queue::open(&store, "domain", &binding)?;
    assert_eq!(queue.get(&mutation.id)?.state, MutationState::Done);
    assert!(queue.get(&mutation.id)?.response.is_some());
    Ok(())
}

#[test]
fn conflicts_keep_dirty_snapshot_and_original_base_and_cannot_be_evicted() -> Result<()> {
    let (_temporary, root, store) = setup()?;
    let binding = state::key("backend");
    let source = edited(&root, "source", b"do not lose this edit")?;
    let mut queue = Queue::open(&store, "domain", &binding)?;
    let mutation = queue.enqueue(request("one", "item"), Some(&source))?;
    queue.start(&mutation.id)?;
    queue.fail(&mutation.id, FailureKind::Conflict, "remote changed")?;
    assert_eq!(queue.status()?.counts["conflict"], 1);
    assert!(queue.status()?.dirty_bytes > 0);
    assert!(queue.discard_snapshot(&mutation.id).is_err());
    assert_eq!(bytes(&queue, &mutation.id)?, b"do not lose this edit");
    let retried = queue.retry(&mutation.id)?;
    assert_eq!(retried.request["base_content_version"], "remote-A");
    assert_eq!(retried.request["base_metadata_version"], "metadata-A");
    assert!(retried.response.is_none());
    assert!(
        queue.complete(&mutation.id, json!({})).is_err(),
        "merely retained local data is not remote completion"
    );
    Ok(())
}

#[test]
fn newer_complete_offline_save_supersedes_only_never_started_covered_intent() -> Result<()> {
    let (_temporary, root, store) = setup()?;
    let binding = state::key("backend");
    let mut queue = Queue::open(&store, "domain", &binding)?;
    let old = queue.enqueue(
        request("old", "item"),
        Some(&edited(&root, "old", b"old bytes")?),
    )?;
    let latest = queue.enqueue(
        request("latest", "item"),
        Some(&edited(&root, "new", b"latest bytes")?),
    )?;
    assert_eq!(queue.get(&old.id)?.state, MutationState::Superseded);
    assert_eq!(
        queue.get(&old.id)?.superseded_by.as_deref(),
        Some(latest.id.as_str())
    );
    assert_eq!(bytes(&queue, &old.id)?, b"old bytes");
    assert_eq!(queue.find_pending_for_item("item").unwrap().id, latest.id);
    assert_eq!(queue.status()?.dirty_bytes, b"latest bytes".len() as u64);
    assert_eq!(
        queue.status()?.superseded_snapshot_bytes,
        b"old bytes".len() as u64
    );
    queue.start(&latest.id)?;
    queue.fail(&latest.id, FailureKind::Offline, "connection lost")?;
    let next = queue.enqueue(
        request("after-ambiguous", "item"),
        Some(&edited(&root, "third", b"third bytes")?),
    )?;
    assert_eq!(
        queue.get(&latest.id)?.state,
        MutationState::Interrupted,
        "ambiguous older attempt must not disappear"
    );
    assert_eq!(queue.get(&next.id)?.state, MutationState::Pending);
    let mut rename = request("rename", "other");
    rename["name"] = json!("renamed.txt");
    let renamed = queue.enqueue(rename, None)?;
    queue.enqueue(
        request("content-only", "other"),
        Some(&edited(&root, "fourth", b"body")?),
    )?;
    assert_eq!(
        queue.get(&renamed.id)?.state,
        MutationState::Pending,
        "content callback must not silently discard an earlier rename"
    );
    Ok(())
}

#[test]
fn pause_survives_restart_but_still_accepts_and_preserves_edits() -> Result<()> {
    let (_temporary, root, store) = setup()?;
    let binding = state::key("backend");
    smart::set_paused(&store, true)?;
    let mut queue = Queue::open(&store, "domain", &binding)?;
    let mutation = queue.enqueue(
        request("paused", "item"),
        Some(&edited(&root, "source", b"saved while paused")?),
    )?;
    assert!(queue.start(&mutation.id).is_err());
    assert!(queue.status()?.paused);
    drop(queue);
    let mut queue = Queue::open(&store, "domain", &binding)?;
    assert!(queue.status()?.paused);
    assert_eq!(queue.get(&mutation.id)?.state, MutationState::Pending);
    smart::set_paused(&store, false)?;
    queue.start(&mutation.id)?;
    assert_eq!(queue.get(&mutation.id)?.state, MutationState::Running);
    Ok(())
}

#[test]
fn idempotency_rejects_different_edit_and_create_transport_ids_do_not_duplicate_work() -> Result<()>
{
    let (_temporary, root, store) = setup()?;
    let binding = state::key("backend");
    let mut queue = Queue::open(&store, "domain", &binding)?;
    let source = edited(&root, "source", b"first")?;
    let first = queue.enqueue(request("same", "item"), Some(&source))?;
    assert_eq!(
        queue.enqueue(request("same", "item"), Some(&source))?.id,
        first.id
    );
    let other = edited(&root, "different", b"different")?;
    assert!(
        queue
            .enqueue(request("same", "item"), Some(&other))
            .is_err()
    );
    assert_eq!(bytes(&queue, &first.id)?, b"first");
    let mut create = json!({"v":1,"op":"create","domain":"domain","client_item_id":"stable-system-id","request_id":"transport-one","name":"file","kind":"file"});
    let new = queue.enqueue(create.clone(), Some(&other))?;
    create["request_id"] = json!("transport-two");
    assert_eq!(queue.enqueue(create, Some(&other))?.id, new.id);
    assert_eq!(queue.list().len(), 2);
    Ok(())
}

#[test]
fn snapshot_lease_prevents_explicit_discard_and_tampered_or_cross_scope_data_cannot_upload()
-> Result<()> {
    let (_temporary, root, store) = setup()?;
    let binding = state::key("backend");
    let mut queue = Queue::open(&store, "domain", &binding)?;
    let mutation = queue.enqueue(
        request("item", "item"),
        Some(&edited(&root, "source", b"original bytes")?),
    )?;
    let lease = queue.snapshot(&mutation.id)?.unwrap();
    let clone = lease.try_clone_file()?;
    queue.cancel(&mutation.id)?;
    assert!(queue.discard_snapshot(&mutation.id).is_err());
    drop(lease);
    assert!(queue.discard_snapshot(&mutation.id).is_err());
    drop(clone);
    queue.discard_snapshot(&mutation.id)?;
    assert!(queue.retry(&mutation.id).is_err());
    let next = queue.enqueue(
        request("second", "other"),
        Some(&edited(&root, "second", b"preserved")?),
    )?;
    let path = queue.snapshot(&next.id)?.unwrap().path().to_owned();
    fs::remove_file(&path)?;
    let outside = root.join("outside");
    fs::write(&outside, b"private unrelated file")?;
    symlink(&outside, &path)?;
    assert!(queue.snapshot(&next.id).is_err());
    assert!(queue.start(&next.id).is_err());
    assert_eq!(fs::read(outside)?, b"private unrelated file");
    assert!(smart::read_only(&store.home, "domain", &state::key("new-backend")).is_err());
    drop(queue);
    assert!(Queue::open(&store, "domain", &state::key("new-backend")).is_err());
    Ok(())
}

#[test]
fn changed_unsynced_create_has_unique_receipt_key_and_retains_every_snapshot() -> Result<()> {
    let (_temporary, root, store) = setup()?;
    let binding = state::key("backend");
    let mut queue = Queue::open(&store, "domain", &binding)?;
    let create = json!({"v":1,"op":"create","domain":"domain","client_item_id":"system-local-id","request_id":"transport-one","parent_id":"root","name":"new.txt","kind":"file"});
    let old = queue.enqueue(
        create.clone(),
        Some(&edited(&root, "old-create", b"old draft")?),
    )?;
    let new = queue.enqueue(
        create.clone(),
        Some(&edited(&root, "new-create", b"latest draft")?),
    )?;
    assert_ne!(old.id, new.id);
    assert_ne!(old.request["client_item_id"], new.request["client_item_id"]);
    assert_eq!(old.request["smart_origin_id"], "system-local-id");
    assert_eq!(new.request["smart_origin_id"], "system-local-id");
    assert_eq!(queue.get(&old.id)?.state, MutationState::Superseded);
    assert_eq!(bytes(&queue, &old.id)?, b"old draft");
    assert_eq!(bytes(&queue, &new.id)?, b"latest draft");
    assert_eq!(
        queue.find_pending_for_item("system-local-id").unwrap().id,
        new.id
    );
    queue.start(&new.id)?;
    queue.fail(&new.id, FailureKind::Offline, "server outcome uncertain")?;
    let last = queue.enqueue(
        create,
        Some(&edited(&root, "last-create", b"latest after disconnect")?),
    )?;
    assert_eq!(queue.get(&new.id)?.state, MutationState::Interrupted);
    assert_eq!(last.state, MutationState::Pending);
    assert_ne!(
        last.request["client_item_id"],
        new.request["client_item_id"]
    );
    assert_eq!(bytes(&queue, &new.id)?, b"latest draft");
    assert_eq!(bytes(&queue, &last.id)?, b"latest after disconnect");
    Ok(())
}

#[test]
fn content_only_create_callback_does_not_supersede_unsent_tag_intent() -> Result<()> {
    let (_temporary, root, store) = setup()?;
    let mut queue = Queue::open(&store, "domain", &state::key("backend"))?;
    let create = json!({"v":1,"op":"create","domain":"domain","client_item_id":"system-local-id","request_id":"transport-one","parent_id":"root","name":"new.txt","kind":"file","tag_data_hex":"0102","extended_attributes":{"com.apple.TextEncoding#S":"7574662d383b313334323137393834"}});
    let prior = queue.enqueue(
        create.clone(),
        Some(&edited(&root, "tagged", b"tagged draft")?),
    )?;
    let mut content_only = create;
    content_only.as_object_mut().unwrap().remove("tag_data_hex");
    content_only
        .as_object_mut()
        .unwrap()
        .remove("extended_attributes");
    let later = queue.enqueue(
        content_only,
        Some(&edited(&root, "content-only", b"new body")?),
    )?;
    assert_eq!(queue.get(&prior.id)?.state, MutationState::Pending);
    assert_eq!(later.state, MutationState::Pending);
    assert_eq!(queue.get(&prior.id)?.request["tag_data_hex"], "0102");
    assert_eq!(bytes(&queue, &prior.id)?, b"tagged draft");
    assert_eq!(bytes(&queue, &later.id)?, b"new body");
    Ok(())
}
