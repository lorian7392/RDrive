//! Authoritative, account-bound provider catalog. Namespace and retained change
//! events are relational rows; mutation receipts remain in the control snapshot.
//! Readers use SQLite snapshots and never acquire the global Store lock.
use super::{Domain, Event, Item, Journal, Record, Request};
use crate::state;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const SCHEMA_VERSION: i64 = 2;
const APPLICATION_ID: i64 = 0x5244_4958; // RDIX

pub(super) fn path(home: &Path, domain: &str) -> PathBuf {
    home.join("provider")
        .join(format!("{}.sqlite3", state::key(domain)))
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn private_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        bail!("provider index must use private owned regular files without links");
    }
    Ok(())
}

fn check_files(db: &Path) -> Result<()> {
    private_file(db)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", db.display()));
        if present(&sidecar)? {
            private_file(&sidecar)?;
        }
    }
    Ok(())
}

fn directory(home: &Path) -> Result<PathBuf> {
    super::private(&home.join("provider"))
}

fn writer_lock(home: &Path, domain: &str) -> Result<File> {
    let directory = directory(home)?;
    let file = directory.join(format!("{}.sqlite3.lock", state::key(domain)));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&file)?;
    private_file(&file)?;
    // This is only a per-domain initialization/publication lock, independent of
    // Store. SQLite supplies transaction isolation for simultaneous readers.
    lock.lock()?;
    Ok(lock)
}

fn open(db: &Path, writable: bool) -> Result<Connection> {
    let parent = db.parent().context("provider index has no directory")?;
    let parent = super::private(parent)?;
    // macOS exposes /var and /tmp aliases. Resolve the validated directory, not
    // the database leaf: SQLite's NOFOLLOW also rejects ancestor aliases.
    let canonical_db = parent.join(db.file_name().context("provider index has no filename")?);
    let db = canonical_db.as_path();
    check_files(db)?;
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = Connection::open_with_flags(
        db,
        flags | OpenFlags::SQLITE_OPEN_NOFOLLOW | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("cannot open provider SQLite index; existing state was preserved")?;
    connection.busy_timeout(Duration::from_secs(3))?;
    connection.pragma_update(None, "trusted_schema", false)?;
    // Opening a v1 database for a read performs its one-time derived-index
    // migration through the same private writer lock used by load/save.
    if !writable {
        let old: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if old == 1 {
            drop(connection);
            let domain = db
                .file_stem()
                .and_then(|s| s.to_str())
                .context("invalid index name")?;
            let lock_path = parent.join(format!("{domain}.sqlite3.lock"));
            let lock = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&lock_path)?;
            private_file(&lock_path)?;
            lock.lock()?;
            drop(open(db, true)?);
            drop(lock);
            return open(db, false);
        }
        connection.pragma_update(None, "query_only", true)?;
    }
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let application: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if version == 1 && application == APPLICATION_ID && writable {
        // Upgrade derived search rows atomically; receipts and retained edits are
        // untouched. A crash before commit leaves the old schema fully usable.
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let migration = (|| -> Result<()> {
            create_search(&connection)?;
            connection.execute("INSERT INTO search_docs(id,name,path,kind) SELECT id,json_extract(item_json,'$.name'),path,json_extract(item_json,'$.kind') FROM items", [])?;
            connection.execute(
                "INSERT INTO search_fts(rowid,name,path) SELECT rowid,name,path FROM search_docs",
                [],
            )?;
            connection.execute(
                "UPDATE meta SET schema_version=?1 WHERE id=1",
                [SCHEMA_VERSION],
            )?;
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            Ok(())
        })();
        match migration {
            Ok(()) => connection.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
    } else if version != SCHEMA_VERSION || application != APPLICATION_ID {
        bail!("unsupported provider index schema; existing state was preserved");
    }
    if writable {
        connection.pragma_update(None, "synchronous", "FULL")?;
    }
    check_files(db)?;
    Ok(connection)
}

fn create_search(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE search_docs(rowid INTEGER PRIMARY KEY,id TEXT NOT NULL UNIQUE,name TEXT NOT NULL,path TEXT NOT NULL,kind TEXT NOT NULL);
        CREATE INDEX search_docs_path ON search_docs(path);
        CREATE VIRTUAL TABLE search_fts USING fts5(name,path,content='search_docs',content_rowid='rowid',tokenize='trigram');")?;
    Ok(())
}

// Namespace operations retain canonical paths for guarded replay. Search and
// Finder filters use the local name/parent projection, including descendants.
fn projected_paths(journal: &Journal) -> Result<BTreeMap<String, String>> {
    fn visit(
        id: &str,
        journal: &Journal,
        done: &mut BTreeMap<String, String>,
        stack: &mut BTreeSet<String>,
    ) -> Result<String> {
        if let Some(path) = done.get(id) {
            return Ok(path.clone());
        }
        if !stack.insert(id.into()) {
            bail!("corrupt provider parent cycle");
        }
        let record = journal
            .items
            .get(id)
            .context("provider projection item missing")?;
        let parent = &record.item.parent_id;
        let canonical_parent = record.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let path = if parent == "root"
            && (canonical_parent.is_empty() || record.item.has_unsynced_changes)
        {
            record.item.name.clone()
        } else if journal.items.contains_key(parent) {
            crate::backend::join(&visit(parent, journal, done, stack)?, &record.item.name)?
        } else {
            record.path.clone()
        };
        stack.remove(id);
        done.insert(id.into(), path.clone());
        Ok(path)
    }
    let mut done = BTreeMap::new();
    let mut stack = BTreeSet::new();
    for id in journal.items.keys() {
        visit(id, journal, &mut done, &mut stack)?;
    }
    Ok(done)
}

fn sync_search(transaction: &Transaction<'_>, journal: &Journal) -> Result<()> {
    let projected = projected_paths(journal)?;
    let mut old = transaction.prepare("SELECT rowid,id,name,path,kind FROM search_docs")?;
    let rows = old
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(old);
    let mut unchanged = BTreeSet::new();
    for (rowid, id, name, path, kind) in rows {
        if journal.items.get(&id).is_some_and(|record| {
            record.item.name == name
                && projected.get(&id) == Some(&path)
                && record.item.kind == kind
        }) {
            unchanged.insert(id);
            continue;
        }
        transaction.execute(
            "INSERT INTO search_fts(search_fts,rowid,name,path) VALUES('delete',?1,?2,?3)",
            params![rowid, name, path],
        )?;
        transaction.execute("DELETE FROM search_docs WHERE rowid=?1", [rowid])?;
    }
    for (id, record) in &journal.items {
        if unchanged.contains(id) {
            continue;
        }
        transaction.execute(
            "INSERT INTO search_docs(id,name,path,kind) VALUES(?1,?2,?3,?4)",
            params![id, record.item.name, projected[id], record.item.kind],
        )?;
        let rowid = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO search_fts(rowid,name,path) VALUES(?1,?2,?3)",
            params![rowid, record.item.name, projected[id]],
        )?;
    }
    Ok(())
}

fn validate_binding(connection: &Connection, domain: &str, binding: &str) -> Result<(u64, String)> {
    let stored: (i64, String, String, i64, String) = connection
        .query_row(
            "SELECT schema_version,domain_id,binding,anchor,pins_json FROM meta WHERE id=1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .context("provider index control is missing or corrupt; existing state was preserved")?;
    if stored.0 != SCHEMA_VERSION {
        bail!("unsupported provider index schema; existing state was preserved");
    }
    if stored.1 != domain || stored.2 != binding {
        bail!("version_conflict: provider domain/backend scope changed; existing state was preserved, use a new domain ID or explicitly repair the index");
    }
    Ok((
        u64::try_from(stored.3).context("invalid provider index anchor")?,
        stored.4,
    ))
}

fn prepare_journal(mut journal: Journal, binding: &str) -> Result<Journal> {
    match journal.binding.as_deref() {
        Some(stored) if stored == binding => (),
        None if journal.items.is_empty() && journal.requests.is_empty() && journal.events.is_empty() && journal.anchor == 0 => journal.binding = Some(binding.into()),
        _ => bail!("version_conflict: provider domain/backend scope changed or journal is unbound; existing state was preserved"),
    }
    if !journal.pins_migrated {
        journal.pins = journal
            .items
            .iter()
            .filter(|(_, record)| record.item.pinned)
            .map(|(id, _)| id.clone())
            .collect();
        journal.pins_migrated = true;
    }
    for record in journal.items.values_mut() {
        record.metadata_known |=
            record.item.tag_data_hex.is_some() || !record.item.extended_attributes.is_empty();
    }
    Ok(journal)
}

fn checked_record(id: String, parent: String, path: String, bytes: String) -> Result<Record> {
    let record: Record = serde_json::from_str(&bytes).context("corrupt provider item record")?;
    if record.item.id != id || record.item.parent_id != parent || record.path != path {
        bail!("corrupt provider item projection");
    }
    Ok(record)
}

fn write_snapshot(
    transaction: &Transaction<'_>,
    domain: &str,
    binding: &str,
    journal: &Journal,
) -> Result<()> {
    if journal.binding.as_deref() != Some(binding) {
        bail!("version_conflict: provider journal binding changed");
    }
    let anchor = i64::try_from(journal.anchor).context("provider anchor exceeds SQLite range")?;
    let mut control = journal.clone();
    control.items.clear();
    control.events.clear();
    control.directory_scans.clear();
    let control_json = serde_json::to_string(&control)?;
    let pins_json = serde_json::to_string(&journal.pins)?;
    transaction.execute("INSERT INTO meta(id,schema_version,domain_id,binding,anchor,control_json,pins_json) VALUES(1,?1,?2,?3,?4,?5,?6)
        ON CONFLICT(id) DO UPDATE SET anchor=excluded.anchor,control_json=excluded.control_json,pins_json=excluded.pins_json
        WHERE meta.anchor<>excluded.anchor OR meta.control_json<>excluded.control_json OR meta.pins_json<>excluded.pins_json",
        params![SCHEMA_VERSION, domain, binding, anchor, control_json, pins_json])?;
    // Temporary membership sets avoid SQL parameter limits and do not rewrite
    // unchanged durable rows, even for a large directory tree.
    transaction.execute_batch(
        "CREATE TEMP TABLE keep_items(id TEXT PRIMARY KEY) WITHOUT ROWID;
        CREATE TEMP TABLE keep_events(anchor INTEGER PRIMARY KEY);
        CREATE TEMP TABLE keep_scans(id TEXT PRIMARY KEY) WITHOUT ROWID;",
    )?;
    {
        let mut keep = transaction.prepare("INSERT INTO keep_items(id) VALUES(?1)")?;
        for (id, record) in &journal.items {
            if id.is_empty()
                || &record.item.id != id
                || crate::backend::normalize(&record.path)? != record.path
            {
                bail!("invalid provider item identity or path");
            }
            keep.execute([id])?;
        }
        transaction.execute("DELETE FROM items WHERE NOT EXISTS(SELECT 1 FROM keep_items WHERE keep_items.id=items.id)", [])?;
        // Release old paths before inserting replacements or exchanging names.
        // These deletes and the replacement rows share this one transaction.
        for (id, record) in &journal.items {
            transaction.execute(
                "DELETE FROM items WHERE id=?1 AND path<>?2",
                params![id, record.path],
            )?;
        }
        let mut upsert = transaction.prepare("INSERT INTO items(id,parent_id,path,record_json,item_json) VALUES(?1,?2,?3,?4,?5)
            ON CONFLICT(id) DO UPDATE SET parent_id=excluded.parent_id,path=excluded.path,record_json=excluded.record_json,item_json=excluded.item_json
            WHERE items.parent_id<>excluded.parent_id OR items.path<>excluded.path OR items.record_json<>excluded.record_json OR items.item_json<>excluded.item_json")?;
        for (id, record) in &journal.items {
            upsert.execute(params![
                id,
                record.item.parent_id,
                record.path,
                serde_json::to_string(record)?,
                serde_json::to_string(&record.item)?
            ])?;
        }
    }
    transaction.execute(
        "DELETE FROM items WHERE NOT EXISTS(SELECT 1 FROM keep_items WHERE keep_items.id=items.id)",
        [],
    )?;
    {
        let mut keep = transaction.prepare("INSERT INTO keep_events(anchor) VALUES(?1)")?;
        let mut upsert = transaction.prepare("INSERT INTO events(anchor,item_json,deleted) VALUES(?1,?2,?3)
            ON CONFLICT(anchor) DO UPDATE SET item_json=excluded.item_json,deleted=excluded.deleted
            WHERE events.item_json IS NOT excluded.item_json OR events.deleted IS NOT excluded.deleted")?;
        for event in &journal.events {
            let event_anchor = i64::try_from(event.anchor)?;
            if event.anchor > journal.anchor || event.item.is_some() == event.deleted.is_some() {
                bail!("invalid retained provider event");
            }
            keep.execute([event_anchor])?;
            upsert.execute(params![
                event_anchor,
                event.item.as_ref().map(serde_json::to_string).transpose()?,
                event.deleted
            ])?;
        }
    }
    transaction.execute("DELETE FROM events WHERE NOT EXISTS(SELECT 1 FROM keep_events WHERE keep_events.anchor=events.anchor)", [])?;
    {
        let mut keep = transaction.prepare("INSERT INTO keep_scans(id) VALUES(?1)")?;
        let mut upsert = transaction.prepare("INSERT INTO directory_scans(id,scanned_at) VALUES(?1,?2)
            ON CONFLICT(id) DO UPDATE SET scanned_at=excluded.scanned_at WHERE directory_scans.scanned_at<>excluded.scanned_at")?;
        for (id, scanned) in &journal.directory_scans {
            keep.execute([id])?;
            upsert.execute(params![id, scanned])?;
        }
    }
    transaction.execute("DELETE FROM directory_scans WHERE NOT EXISTS(SELECT 1 FROM keep_scans WHERE keep_scans.id=directory_scans.id)", [])?;
    transaction
        .execute_batch("DROP TABLE keep_items; DROP TABLE keep_events; DROP TABLE keep_scans;")?;
    sync_search(transaction, journal)?;
    Ok(())
}

fn initialize(home: &Path, domain: &str, binding: &str, journal: &Journal) -> Result<()> {
    let directory = directory(home)?;
    let destination = path(home, domain);
    let temporary = directory.join(format!(".rdrive-index-{}.sqlite3", uuid::Uuid::new_v4()));
    let temporary_file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        let mut connection = Connection::open_with_flags(
            &temporary,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction()?;
        transaction.execute_batch("CREATE TABLE meta(id INTEGER PRIMARY KEY CHECK(id=1),schema_version INTEGER NOT NULL,domain_id TEXT NOT NULL,binding TEXT NOT NULL,anchor INTEGER NOT NULL CHECK(anchor>=0),pins_json TEXT NOT NULL,control_json TEXT NOT NULL);
            CREATE TABLE items(id TEXT PRIMARY KEY,parent_id TEXT NOT NULL,path TEXT NOT NULL UNIQUE,item_json TEXT NOT NULL,record_json TEXT NOT NULL) WITHOUT ROWID;
            CREATE INDEX items_parent_id ON items(parent_id,id);
            CREATE TABLE events(anchor INTEGER PRIMARY KEY,item_json TEXT,deleted TEXT);
            CREATE TABLE directory_scans(id TEXT PRIMARY KEY,scanned_at INTEGER NOT NULL) WITHOUT ROWID;")?;
        create_search(&transaction)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        write_snapshot(&transaction, domain, binding, journal)?;
        transaction.commit()?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        connection.close().map_err(|(_, error)| error)?;
        temporary_file.sync_all()?;
        // The initialization lock prevents two migrations from publishing over
        // one another. Only a complete, committed database becomes authoritative.
        if present(&destination)? {
            bail!("provider index appeared during initialization");
        }
        fs::rename(&temporary, &destination)?;
        File::open(&directory)?.sync_all()?;
        check_files(&destination)?;
        Ok(())
    })();
    if result.is_err() {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", temporary.display())));
        }
    }
    result
}

pub(super) fn load(
    home: &Path,
    domain: &str,
    binding: &str,
    legacy: &Path,
) -> Result<Option<Journal>> {
    let db = path(home, domain);
    if !present(&db)? {
        let _lock = writer_lock(home, domain)?;
        if !present(&db)? {
            let initial = if present(legacy)? {
                private_file(legacy)?;
                serde_json::from_slice(&fs::read(legacy)?)
                    .context("invalid legacy provider journal; existing state was preserved")?
            } else {
                Journal::default()
            };
            let journal = prepare_journal(initial, binding)?;
            initialize(home, domain, binding, &journal)?;
        }
    }
    let mut connection = open(&db, false)?;
    let transaction = connection.transaction()?;
    let (anchor, pins) = validate_binding(&transaction, domain, binding)?;
    let control: String =
        transaction.query_row("SELECT control_json FROM meta WHERE id=1", [], |row| {
            row.get(0)
        })?;
    let mut journal: Journal =
        serde_json::from_str(&control).context("corrupt provider index control")?;
    if journal.binding.as_deref() != Some(binding)
        || journal.anchor != anchor
        || journal.pins != serde_json::from_str::<BTreeSet<String>>(&pins)?
        || !journal.items.is_empty()
        || !journal.events.is_empty()
        || !journal.directory_scans.is_empty()
    {
        bail!("corrupt provider index control projection");
    }
    {
        let mut statement =
            transaction.prepare("SELECT id,parent_id,path,record_json FROM items ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, parent, path, bytes) = row?;
            journal
                .items
                .insert(id.clone(), checked_record(id, parent, path, bytes)?);
        }
    }
    {
        let mut statement =
            transaction.prepare("SELECT anchor,item_json,deleted FROM events ORDER BY anchor")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (anchor, item, deleted) = row?;
            journal.events.push(Event {
                anchor: u64::try_from(anchor)?,
                item: item.map(|bytes| serde_json::from_str(&bytes)).transpose()?,
                deleted,
            });
        }
    }
    {
        let mut statement = transaction.prepare("SELECT id,scanned_at FROM directory_scans")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (id, scanned) = row?;
            journal.directory_scans.insert(id, scanned);
        }
    }
    transaction.commit()?;
    Ok(Some(journal))
}

pub(super) fn save(home: &Path, domain: &str, binding: &str, journal: &Journal) -> Result<()> {
    let _lock = writer_lock(home, domain)?;
    let db = path(home, domain);
    if !present(&db)? {
        return initialize(home, domain, binding, journal);
    }
    let mut connection = open(&db, true)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    validate_binding(&transaction, domain, binding)?;
    write_snapshot(&transaction, domain, binding, journal)?;
    transaction.commit()?;
    check_files(&db)?;
    Ok(())
}

pub(super) fn query(
    home: &Path,
    domain: &Domain,
    binding: &str,
    request: &Request,
) -> Result<Option<Value>> {
    let db = path(home, &domain.id);
    if !present(&db)? {
        return Ok(None);
    }
    let mut connection = open(&db, false)?;
    let transaction = connection.transaction()?;
    let (anchor, pins) = validate_binding(&transaction, &domain.id, binding)?;
    let config: crate::config::Config = state::read_json(&home.join("config.json"))?;
    let selector = crate::filtering::Selector::new(&config, &domain.profile)?;
    let visible = |item: &Item, path: &str| -> Result<bool> {
        Ok(item.sync_state.as_deref() != Some("queued_delete")
            && (!domain.smart || selector.allowed(path, item.kind == "directory")?))
    };
    let response = match request.op.as_str() {
        "item" => {
            let id = request.id.as_deref().unwrap_or("root");
            let item = if id == "root" {
                let pins: BTreeSet<String> = serde_json::from_str(&pins)?;
                let mut root = super::root_item(domain);
                root.pinned = pins.contains("root");
                root.metadata_version = format!("root:{}", root.pinned);
                root.sync_state = Some(if root.pinned { "pinned" } else { "online" }.into());
                root
            } else {
                let (bytes,path): (String,String) = transaction
                    .query_row("SELECT i.item_json,d.path FROM items i JOIN search_docs d ON d.id=i.id WHERE i.id=?1", [id], |row| {
                        Ok((row.get(0)?,row.get(1)?))
                    })
                    .optional()?
                    .context("no_such_item")?;
                let item: Item =
                    serde_json::from_str(&bytes).context("corrupt provider item projection")?;
                if !visible(&item, &path)? {
                    bail!("no_such_item");
                }
                if item.id != id {
                    bail!("corrupt provider item identity");
                }
                item
            };
            json!({"item":item})
        }
        "list" => {
            let parent = request.parent_id.as_deref().unwrap_or("root");
            let sql = if parent == "working_set" {
                "SELECT i.item_json,d.path FROM items i JOIN search_docs d ON d.id=i.id ORDER BY i.id"
            } else {
                "SELECT i.item_json,d.path FROM items i JOIN search_docs d ON d.id=i.id WHERE i.parent_id=?1 ORDER BY i.id"
            };
            let mut statement = transaction.prepare(sql)?;
            let mut rows = if parent == "working_set" {
                statement.query([])?
            } else {
                statement.query([parent])?
            };
            let mut items = vec![];
            while let Some(row) = rows.next()? {
                let bytes: String = row.get(0)?;
                let item: Item =
                    serde_json::from_str(&bytes).context("corrupt provider item projection")?;
                let path: String = row.get(1)?;
                if visible(&item, &path)? {
                    items.push(item);
                }
            }
            json!({"items":items,"anchor":anchor.to_string(),"smart":domain.smart})
        }
        "changes" => {
            if request.anchor.as_deref() == Some("current") {
                json!({"items":[],"deleted":[],"anchor":anchor.to_string(),"more":false})
            } else {
                let since = request.anchor.as_deref().unwrap_or("0").parse::<u64>()?;
                let first: Option<i64> =
                    transaction
                        .query_row("SELECT min(anchor) FROM events", [], |row| row.get(0))?;
                if since > anchor
                    || first
                        .is_some_and(|first| since > 0 && since.saturating_add(1) < first as u64)
                {
                    bail!("sync_anchor_expired");
                }
                let mut items = BTreeMap::new();
                let mut deleted = BTreeSet::new();
                let mut statement = transaction.prepare(
                    "SELECT item_json,deleted FROM events WHERE anchor>?1 ORDER BY anchor",
                )?;
                let mut rows = statement.query([i64::try_from(since)?])?;
                while let Some(row) = rows.next()? {
                    if let Some(bytes) = row.get::<_, Option<String>>(0)? {
                        let item: Item = serde_json::from_str(&bytes)?;
                        let path: Option<String> = if item.id == "root" {
                            Some(String::new())
                        } else {
                            transaction
                                .query_row(
                                    "SELECT path FROM search_docs WHERE id=?1",
                                    [&item.id],
                                    |row| row.get(0),
                                )
                                .optional()?
                        };
                        if path
                            .as_deref()
                            .map(|path| visible(&item, path))
                            .transpose()?
                            .unwrap_or(false)
                        {
                            deleted.remove(&item.id);
                            items.insert(item.id.clone(), item);
                        } else {
                            items.remove(&item.id);
                            deleted.insert(item.id.clone());
                        }
                    }
                    if let Some(id) = row.get::<_, Option<String>>(1)? {
                        items.remove(&id);
                        deleted.insert(id);
                    }
                }
                json!({"items":items.values().collect::<Vec<_>>(),"deleted":deleted,"anchor":anchor.to_string(),"more":false})
            }
        }
        _ => bail!("unsupported catalog operation"),
    };
    transaction.commit()?;
    Ok(Some(response))
}

fn lookup_by(
    home: &Path,
    domain: &str,
    binding: &str,
    key: &str,
    by_path: bool,
) -> Result<Option<Record>> {
    let db = path(home, domain);
    if !present(&db)? {
        return Ok(None);
    }
    let mut connection = open(&db, false)?;
    let transaction = connection.transaction()?;
    validate_binding(&transaction, domain, binding)?;
    let sql = if by_path {
        "SELECT id,parent_id,path,record_json FROM items WHERE path=?1"
    } else {
        "SELECT id,parent_id,path,record_json FROM items WHERE id=?1"
    };
    let row = transaction
        .query_row(sql, [key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .optional()?;
    let result = row
        .map(|(id, parent, path, bytes)| checked_record(id, parent, path, bytes))
        .transpose()?;
    transaction.commit()?;
    Ok(result)
}

pub(super) fn lookup(home: &Path, domain: &str, binding: &str, id: &str) -> Result<Option<Record>> {
    lookup_by(home, domain, binding, id, false)
}
pub(super) fn lookup_path(
    home: &Path,
    domain: &str,
    binding: &str,
    relative: &str,
) -> Result<Option<Record>> {
    lookup_by(home, domain, binding, relative, true)
}

pub(super) fn directory_scan(
    home: &Path,
    domain: &str,
    binding: &str,
    id: &str,
) -> Result<Option<i64>> {
    let db = path(home, domain);
    if !present(&db)? {
        return Ok(None);
    }
    let mut connection = open(&db, false)?;
    let transaction = connection.transaction()?;
    validate_binding(&transaction, domain, binding)?;
    let result = transaction
        .query_row(
            "SELECT scanned_at FROM directory_scans WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    transaction.commit()?;
    Ok(result)
}

pub(super) fn status(home: &Path, domain: &str, binding: &str) -> Result<Value> {
    let db = path(home, domain);
    if !present(&db)? {
        return Ok(
            json!({"initialized":false,"items":0,"files":0,"directories":0,"scanned":0,"pending":0,"anchor":"0","database":db,"schema_version":SCHEMA_VERSION}),
        );
    }
    let mut connection = open(&db, false)?;
    let transaction = connection.transaction()?;
    let (anchor, _) = validate_binding(&transaction, domain, binding)?;
    let items: i64 = transaction.query_row("SELECT count(*) FROM items", [], |row| row.get(0))?;
    let directories: i64 = transaction.query_row(
        "SELECT count(*) FROM items WHERE json_extract(item_json,'$.kind')='directory'",
        [],
        |row| row.get(0),
    )?;
    let scanned: i64 = transaction.query_row("SELECT count(*) FROM directory_scans WHERE id='root' OR id IN (SELECT id FROM items WHERE json_extract(item_json,'$.kind')='directory')",[],|row|row.get(0))?;
    transaction.commit()?;
    Ok(
        json!({"initialized":true,"items":items,"files":items-directories,"directories":directories,"scanned":scanned,"pending":(directories+1-scanned).max(0),"anchor":anchor.to_string(),"database":db,"schema_version":SCHEMA_VERSION}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture() -> (tempfile::TempDir, PathBuf, Domain, Journal) {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("state");
        super::super::private(&home).unwrap();
        directory(&home).unwrap();
        let domain = Domain {
            id: "index-test".into(),
            profile: "test".into(),
            remote: String::new(),
            display_name: "Index".into(),
            repository: false,
            local_root: None,
            smart: true,
        };
        let journal = Journal {
            binding: Some("bound".into()),
            pins_migrated: true,
            ..Default::default()
        };
        (temporary, home, domain, journal)
    }

    fn record(domain: &Domain, id: &str, parent: &str, path: &str) -> Record {
        let mut item = super::super::root_item(domain);
        item.id = id.into();
        item.parent_id = parent.into();
        item.name = path.rsplit('/').next().unwrap().into();
        item.kind = "file".into();
        Record {
            path: path.into(),
            item,
            entry: None,
            strong: None,
            client_id: None,
            metadata_known: false,
            metadata_checked: 0,
        }
    }

    fn request(op: &str, parent: &str) -> Request {
        serde_json::from_value(json!({"v":1,"op":op,"domain":"index-test","parent_id":parent}))
            .unwrap()
    }

    #[test]
    fn migration_is_atomic_authoritative_and_preserves_receipts_pins_events_and_legacy_bytes() {
        let (_temporary, home, domain, mut journal) = fixture();
        let legacy = home
            .join("provider")
            .join(format!("{}.json", state::key(&domain.id)));
        let file = record(&domain, "file", "root", "file.txt");
        journal.items.insert("file".into(), file.clone());
        journal.pins.insert("root".into());
        journal.anchor = 3;
        journal.events.push(Event {
            anchor: 3,
            item: Some(file.item.clone()),
            deleted: None,
        });
        journal.requests.insert(
            "receipt".into(),
            serde_json::from_value(
                json!({"fingerprint":"protected","response":{"item":file.item}}),
            )
            .unwrap(),
        );
        journal.directory_scans.insert("root".into(), 42);
        state::atomic_json(&legacy, &journal).unwrap();
        let original = fs::read(&legacy).unwrap();
        let loaded = load(&home, &domain.id, "bound", &legacy).unwrap().unwrap();
        assert_eq!(loaded.anchor, 3);
        assert_eq!(loaded.items.len(), 1);
        assert!(loaded.pins.contains("root"));
        assert_eq!(loaded.directory_scans["root"], 42);
        assert!(loaded.requests["receipt"].response.is_some());
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(fs::read(&legacy).unwrap(), original);
        state::atomic_json(&legacy, &Journal::default()).unwrap();
        assert_eq!(
            load(&home, &domain.id, "bound", &legacy)
                .unwrap()
                .unwrap()
                .anchor,
            3
        );
        let before = fs::read(path(&home, &domain.id)).unwrap();
        assert!(load(&home, &domain.id, "different", &legacy)
            .unwrap_err()
            .to_string()
            .contains("scope changed"));
        assert_eq!(fs::read(path(&home, &domain.id)).unwrap(), before);
        assert!(query(&home, &domain, "different", &request("list", "root")).is_err());
        assert!(save(&home, &domain.id, "different", &journal).is_err());
    }

    #[test]
    fn failed_initial_migration_and_failed_update_never_publish_partial_snapshots() {
        let (_temporary, home, domain, mut journal) = fixture();
        let legacy = home.join("provider").join("legacy.json");
        journal
            .items
            .insert("a".into(), record(&domain, "a", "root", "same"));
        journal
            .items
            .insert("b".into(), record(&domain, "b", "root", "same"));
        state::atomic_json(&legacy, &journal).unwrap();
        let original = fs::read(&legacy).unwrap();
        assert!(load(&home, &domain.id, "bound", &legacy).is_err());
        assert!(!path(&home, &domain.id).exists());
        assert_eq!(fs::read(&legacy).unwrap(), original);
        journal.items.remove("b");
        save(&home, &domain.id, "bound", &journal).unwrap();
        journal.anchor = 1;
        journal.directory_scans.insert("root".into(), 99);
        journal.pins.insert("root".into());
        journal
            .items
            .insert("b".into(), record(&domain, "b", "root", "same"));
        assert!(save(&home, &domain.id, "bound", &journal).is_err());
        let recovered = load(&home, &domain.id, "bound", &legacy).unwrap().unwrap();
        assert_eq!(recovered.anchor, 0);
        assert_eq!(recovered.items.len(), 1);
        assert!(recovered.pins.is_empty());
        assert!(recovered.directory_scans.is_empty());
    }

    #[test]
    fn parent_query_uses_index_and_never_deserializes_unrelated_corrupt_rows() {
        let (_temporary, home, domain, mut journal) = fixture();
        journal
            .items
            .insert("a".into(), record(&domain, "a", "one", "one/a"));
        journal
            .items
            .insert("b".into(), record(&domain, "b", "other", "other/b"));
        save(&home, &domain.id, "bound", &journal).unwrap();
        let connection = open(&path(&home, &domain.id), true).unwrap();
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT item_json FROM items WHERE parent_id=?1 ORDER BY id",
                ["one"],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("items_parent_id"), "{plan}");
        connection
            .execute(
                "UPDATE items SET record_json='{',item_json='{' WHERE id='b'",
                [],
            )
            .unwrap();
        drop(connection);
        let response = query(&home, &domain, "bound", &request("list", "one"))
            .unwrap()
            .unwrap();
        assert_eq!(response["items"].as_array().unwrap().len(), 1);
        assert_eq!(response["items"][0]["id"], "a");
        assert_eq!(
            lookup(&home, &domain.id, "bound", "a")
                .unwrap()
                .unwrap()
                .path,
            "one/a"
        );
        assert_eq!(
            lookup_path(&home, &domain.id, "bound", "one/a")
                .unwrap()
                .unwrap()
                .item
                .id,
            "a"
        );
        assert!(query(&home, &domain, "bound", &request("list", "other")).is_err());
        assert!(load(&home, &domain.id, "bound", &home.join("absent")).is_err());
    }

    #[test]
    fn unchanged_save_does_not_rewrite_durable_rows_and_one_change_updates_one_item() {
        let (_temporary, home, domain, mut journal) = fixture();
        for id in ["a", "b"] {
            journal
                .items
                .insert(id.into(), record(&domain, id, "root", id));
        }
        journal.directory_scans.insert("root".into(), 123);
        save(&home, &domain.id, "bound", &journal).unwrap();
        let connection = open(&path(&home, &domain.id), true).unwrap();
        connection.execute_batch("CREATE TABLE audit(changes INTEGER NOT NULL); INSERT INTO audit VALUES(0);
            CREATE TRIGGER audit_item_update AFTER UPDATE ON items BEGIN UPDATE audit SET changes=changes+1; END;
            CREATE TRIGGER audit_item_insert AFTER INSERT ON items BEGIN UPDATE audit SET changes=changes+1; END;
            CREATE TRIGGER audit_item_delete AFTER DELETE ON items BEGIN UPDATE audit SET changes=changes+1; END;
            CREATE TRIGGER audit_meta_update AFTER UPDATE ON meta BEGIN UPDATE audit SET changes=changes+1; END;
            CREATE TRIGGER audit_scan_update AFTER UPDATE ON directory_scans BEGIN UPDATE audit SET changes=changes+1; END;").unwrap();
        drop(connection);
        save(&home, &domain.id, "bound", &journal).unwrap();
        let changes = || {
            open(&path(&home, &domain.id), false)
                .unwrap()
                .query_row("SELECT changes FROM audit", [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(changes(), 0);
        journal.items.get_mut("a").unwrap().item.size = 3;
        save(&home, &domain.id, "bound", &journal).unwrap();
        assert_eq!(changes(), 1);
    }

    #[test]
    fn replacements_and_exchanged_paths_are_published_in_one_transaction() {
        let (_temporary, home, domain, mut journal) = fixture();
        journal
            .items
            .insert("a".into(), record(&domain, "a", "root", "left"));
        journal
            .items
            .insert("b".into(), record(&domain, "b", "root", "right"));
        save(&home, &domain.id, "bound", &journal).unwrap();
        journal.items.get_mut("a").unwrap().path = "right".into();
        journal.items.get_mut("b").unwrap().path = "left".into();
        save(&home, &domain.id, "bound", &journal).unwrap();
        assert_eq!(
            lookup_path(&home, &domain.id, "bound", "left")
                .unwrap()
                .unwrap()
                .item
                .id,
            "b"
        );
        journal.items.remove("b");
        journal
            .items
            .insert("c".into(), record(&domain, "c", "root", "left"));
        save(&home, &domain.id, "bound", &journal).unwrap();
        assert_eq!(
            lookup_path(&home, &domain.id, "bound", "left")
                .unwrap()
                .unwrap()
                .item
                .id,
            "c"
        );
    }

    #[test]
    fn changes_keep_anchor_expiry_deduplication_root_policy_and_snapshot_isolation() {
        let (_temporary, home, domain, mut journal) = fixture();
        let first = record(&domain, "a", "root", "a");
        let mut latest = first.item.clone();
        latest.size = 4;
        let mut current = record(&domain, "a", "root", "a");
        current.item = latest.clone();
        journal.items.insert("a".into(), current);
        journal.pins.insert("root".into());
        journal.anchor = 8;
        journal.events = vec![
            Event {
                anchor: 5,
                item: Some(first.item),
                deleted: None,
            },
            Event {
                anchor: 6,
                item: None,
                deleted: Some("a".into()),
            },
            Event {
                anchor: 7,
                item: Some(latest),
                deleted: None,
            },
            Event {
                anchor: 8,
                item: None,
                deleted: Some("b".into()),
            },
        ];
        save(&home, &domain.id, "bound", &journal).unwrap();
        let mut changes = request("changes", "root");
        changes.anchor = Some("3".into());
        assert!(query(&home, &domain, "bound", &changes)
            .unwrap_err()
            .to_string()
            .contains("sync_anchor_expired"));
        changes.anchor = Some("4".into());
        let response = query(&home, &domain, "bound", &changes).unwrap().unwrap();
        assert_eq!(response["items"][0]["size"], 4);
        assert_eq!(response["deleted"], json!(["b"]));
        assert_eq!(response["anchor"], "8");
        let response = query(&home, &domain, "bound", &request("item", "root"))
            .unwrap()
            .unwrap();
        assert_eq!(response["item"]["pinned"], true);
        let mut connection = open(&path(&home, &domain.id), false).unwrap();
        let transaction = connection.transaction().unwrap();
        assert_eq!(
            validate_binding(&transaction, &domain.id, "bound")
                .unwrap()
                .0,
            8
        );
        journal.anchor = 9;
        save(&home, &domain.id, "bound", &journal).unwrap();
        assert_eq!(
            validate_binding(&transaction, &domain.id, "bound")
                .unwrap()
                .0,
            8
        );
        transaction.commit().unwrap();
        changes.anchor = Some("current".into());
        assert_eq!(
            query(&home, &domain, "bound", &changes).unwrap().unwrap()["anchor"],
            "9"
        );
    }

    #[test]
    fn corrupt_unknown_schema_and_symlink_files_fail_closed_without_legacy_fallback() {
        let (_temporary, home, domain, journal) = fixture();
        let db = path(&home, &domain.id);
        let legacy = home.join("provider/legacy.json");
        state::atomic_json(&legacy, &journal).unwrap();
        let outside = home.join("outside");
        fs::write(&outside, b"untouched").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&outside, &db).unwrap();
        assert!(load(&home, &domain.id, "bound", &legacy).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        fs::remove_file(&db).unwrap();
        save(&home, &domain.id, "bound", &journal).unwrap();
        let sidecar = PathBuf::from(format!("{}-wal", db.display()));
        symlink(&outside, &sidecar).unwrap();
        assert!(query(&home, &domain, "bound", &request("list", "root")).is_err());
        fs::remove_file(sidecar).unwrap();
        let connection = open(&db, true).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        assert!(load(&home, &domain.id, "bound", &legacy)
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
        fs::remove_file(&db).unwrap();
        fs::write(&db, b"not a sqlite database").unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&home, &domain.id, "bound", &legacy).is_err());
        assert_eq!(fs::read(&db).unwrap(), b"not a sqlite database");
    }

    #[test]
    fn absent_and_unbound_state_and_scan_status_have_explicit_semantics() {
        let (_temporary, home, domain, mut journal) = fixture();
        let legacy = home.join("provider/legacy.json");
        assert!(query(&home, &domain, "bound", &request("list", "root"))
            .unwrap()
            .is_none());
        assert_eq!(
            status(&home, &domain.id, "bound").unwrap()["initialized"],
            false
        );
        assert!(!path(&home, &domain.id).exists());
        journal.binding = None;
        journal.anchor = 1;
        state::atomic_json(&legacy, &journal).unwrap();
        assert!(load(&home, &domain.id, "bound", &legacy).is_err());
        assert!(!path(&home, &domain.id).exists());
        journal.anchor = 0;
        state::atomic_json(&legacy, &journal).unwrap();
        assert_eq!(
            load(&home, &domain.id, "bound", &legacy)
                .unwrap()
                .unwrap()
                .binding
                .as_deref(),
            Some("bound")
        );
        journal.binding = Some("bound".into());
        let mut folder = record(&domain, "folder", "root", "folder");
        folder.item.kind = "directory".into();
        journal.items.insert("folder".into(), folder);
        journal.directory_scans.insert("root".into(), 42);
        save(&home, &domain.id, "bound", &journal).unwrap();
        assert_eq!(
            directory_scan(&home, &domain.id, "bound", "root").unwrap(),
            Some(42)
        );
        let status = status(&home, &domain.id, "bound").unwrap();
        assert_eq!(status["directories"], 1);
        assert_eq!(status["scanned"], 1);
        assert_eq!(status["pending"], 1);
        let connection = open(&path(&home, &domain.id), true).unwrap();
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let sync: i64 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(sync, 2);
        check_files(&path(&home, &domain.id)).unwrap();
        assert_eq!(
            fs::metadata(path(&home, &domain.id)).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn missing_database_initialization_is_serialized_and_never_overwrites_a_published_catalog() {
        let (_temporary, home, domain, mut journal) = fixture();
        let legacy = home.join("provider/absent.json");
        // A fast request observed no database, then another writer completed a
        // scan before the request could initialize it. The late reader must
        // reload that published state rather than save its stale empty default.
        assert!(query(&home, &domain, "bound", &request("list", "root"))
            .unwrap()
            .is_none());
        let lock = writer_lock(&home, &domain.id).unwrap();
        let reader_home = home.clone();
        let reader_domain = domain.id.clone();
        let reader_legacy = legacy.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            send.send(()).unwrap();
            load(&reader_home, &reader_domain, "bound", &reader_legacy)
                .unwrap()
                .unwrap()
        });
        receive.recv().unwrap();
        journal
            .items
            .insert("saved".into(), record(&domain, "saved", "root", "saved"));
        initialize(&home, &domain.id, "bound", &journal).unwrap();
        drop(lock);
        assert!(reader.join().unwrap().items.contains_key("saved"));
        assert!(load(&home, &domain.id, "bound", &legacy)
            .unwrap()
            .unwrap()
            .items
            .contains_key("saved"));
        let (_temporary, home, domain, _journal) = fixture();
        assert!(load(&home, &domain.id, "bound", &home.join("missing"))
            .unwrap()
            .is_some());
        assert!(query(&home, &domain, "bound", &request("list", "root"))
            .unwrap()
            .is_some());
    }
    #[test]
    fn local_search_handles_unicode_literal_syntax_filters_pages_and_incremental_paths() {
        let (_temporary, home, domain, mut journal) = fixture();
        for (id, path) in [
            ("a", "docs/年度报告.txt"),
            ("b", "docs/report-one.txt"),
            ("c", "docs/report-two.txt"),
            ("d", "secret/report.txt"),
            ("e", "a%_literal.txt"),
        ] {
            journal
                .items
                .insert(id.into(), record(&domain, id, "root", path));
        }
        save(&home, &domain.id, "bound", &journal).unwrap();
        let mut config = crate::config::Config::default();
        config.filters.exclude = vec!["secret/".into()];
        let result = search(&home, &domain, "bound", &config, "report", "", None, 1, 0).unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 1);
        assert_eq!(result["more"], true);
        assert_eq!(result["engine"], "fts5-trigram");
        let page = search(&home, &domain, "bound", &config, "report", "", None, 1, 1).unwrap();
        assert_ne!(result["items"][0]["path"], page["items"][0]["path"]);
        assert_eq!(page["more"], false);
        assert_eq!(
            search(
                &home,
                &domain,
                "bound",
                &config,
                "报告",
                "docs",
                Some("file"),
                10,
                0
            )
            .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search(&home, &domain, "bound", &config, "%_", "", None, 10, 0).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search(&home, &domain, "bound", &config, "OR *", "", None, 10, 0).unwrap()["items"],
            json!([])
        );
        journal.items.remove("b");
        journal.items.get_mut("c").unwrap().path = "docs/renamed.txt".into();
        journal.items.get_mut("c").unwrap().item.name = "renamed.txt".into();
        save(&home, &domain.id, "bound", &journal).unwrap();
        assert_eq!(
            search(&home, &domain, "bound", &config, "report", "", None, 10, 0).unwrap()["items"],
            json!([])
        );
        assert_eq!(
            search(&home, &domain, "bound", &config, "renamed", "docs", None, 10, 0).unwrap()
                ["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(search(&home, &domain, "bound", &config, "x", "", None, 0, 0).is_err());
        assert!(search(&home, &domain, "wrong", &config, "x", "", None, 1, 0).is_err());
    }
    #[test]
    fn search_tracks_pending_reparent_and_descendants_without_rewriting_remote_paths() {
        let (_temporary, home, domain, mut journal) = fixture();
        let mut folder = record(&domain, "folder", "root", "original");
        folder.item.kind = "directory".into();
        folder.item.name = "renamed".into();
        folder.item.has_unsynced_changes = true;
        folder.item.sync_state = Some("queued".into());
        journal.items.insert("folder".into(), folder);
        journal.items.insert(
            "child".into(),
            record(&domain, "child", "folder", "original/child.txt"),
        );
        save(&home, &domain.id, "bound", &journal).unwrap();
        let found = search(
            &home,
            &domain,
            "bound",
            &crate::config::Config::default(),
            "child",
            "renamed",
            None,
            10,
            0,
        )
        .unwrap();
        assert_eq!(found["items"][0]["path"], "renamed/child.txt");
        assert_eq!(
            lookup(&home, &domain.id, "bound", "child")
                .unwrap()
                .unwrap()
                .path,
            "original/child.txt"
        );
        assert!(search(
            &home,
            &domain,
            "bound",
            &crate::config::Config::default(),
            "child",
            "original",
            None,
            10,
            0
        )
        .unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn schema_one_upgrade_preserves_canonical_records_receipts_events_and_pins() {
        let (_temporary, home, domain, mut journal) = fixture();
        journal.items.insert(
            "a".into(),
            record(&domain, "a", "root", "old/searchable.txt"),
        );
        journal.pins.insert("a".into());
        journal.anchor = 9;
        save(&home, &domain.id, "bound", &journal).unwrap();
        let db = path(&home, &domain.id);
        let conn = open(&db, true).unwrap();
        conn.execute_batch("DROP TABLE search_fts; DROP TABLE search_docs; UPDATE meta SET schema_version=1; PRAGMA user_version=1;").unwrap();
        drop(conn);
        let expected = serde_json::to_value(&journal).unwrap();
        // The first reader performs the atomic one-time upgrade.
        let result = search(
            &home,
            &domain,
            "bound",
            &crate::config::Config::default(),
            "searchable",
            "",
            None,
            10,
            0,
        )
        .unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 1);
        let loaded = load(&home, &domain.id, "bound", &home.join("missing.json"))
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_value(loaded).unwrap(), expected);
        assert_eq!(
            open(&db, false)
                .unwrap()
                .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            2
        );
    }
}

/// Offline local name/path substring search. Result pages never contact storage.
pub(super) fn search(
    home: &Path,
    domain: &Domain,
    binding: &str,
    config: &crate::config::Config,
    query: &str,
    scope: &str,
    kind: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Value> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 512 || query.chars().any(char::is_control) {
        bail!("search requires 1 to 512 printable characters");
    }
    if limit == 0 || limit > 1000 || offset > 1_000_000 {
        bail!("search limit must be 1..1000 and offset at most 1000000");
    }
    if kind.is_some_and(|kind| !["file", "directory"].contains(&kind)) {
        bail!("kind must be file or directory");
    }
    let scope = crate::backend::normalize(scope)?;
    let selector = crate::filtering::Selector::new(config, &domain.profile)?;
    let db = path(home, &domain.id);
    if !present(&db)? {
        return Ok(
            json!({"domain":domain.id,"query":query,"items":[],"initialized":false,"offline":true,"more":false,"offset":offset}),
        );
    }
    let mut connection = open(&db, false)?;
    let transaction = connection.transaction()?;
    let (anchor, _) = validate_binding(&transaction, &domain.id, binding)?;
    let trigram = query.chars().count() >= 3;
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = if trigram {
        format!("\"{}\"", query.replace('"', "\"\""))
    } else {
        format!("%{escaped}%")
    };
    let predicate = if trigram {
        "d.rowid IN (SELECT rowid FROM search_fts WHERE search_fts MATCH ?1)"
    } else {
        "(d.name LIKE ?1 ESCAPE '\\' OR d.path LIKE ?1 ESCAPE '\\')"
    };
    let sql=format!("SELECT i.id,i.parent_id,i.path,i.record_json,d.path FROM search_docs d JOIN items i ON i.id=d.id WHERE {predicate} AND (?2='' OR d.path=?2 OR substr(d.path,1,length(?2)+1)=?2||'/') AND (?3 IS NULL OR d.kind=?3) ORDER BY d.path COLLATE NOCASE,d.id");
    let mut statement = transaction.prepare(&sql)?;
    let mut rows = statement.query(params![pattern, scope, kind])?;
    let mut items = vec![];
    let mut visible = 0usize;
    let mut more = false;
    while let Some(row) = rows.next()? {
        let record = checked_record(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)?;
        let display_path: String = row.get(4)?;
        if record.item.sync_state.as_deref() == Some("queued_delete")
            || (domain.smart
                && !selector.allowed(&display_path, record.item.kind == "directory")?)
        {
            continue;
        }
        if visible < offset {
            visible += 1;
            continue;
        }
        if items.len() == limit {
            more = true;
            break;
        }
        items.push(json!({"path":display_path,"item":record.item}));
    }
    drop(rows);
    drop(statement);
    transaction.commit()?;
    Ok(
        json!({"domain":domain.id,"query":query,"scope":scope,"items":items,"initialized":true,"offline":true,"engine":if trigram {"fts5-trigram"}else{"sqlite-like"},"anchor":anchor.to_string(),"offset":offset,"more":more,"next_offset":if more {Some(offset+limit)}else{None}}),
    )
}
