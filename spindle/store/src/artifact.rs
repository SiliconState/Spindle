//! Private SQLite-backed immutable objects. Objects are retained while any revision,
//! receipt or rewound task reaches them (directly or through a workspace manifest),
//! while a host stage pins them, or while a strict receipt names them as retained
//! output; `Store::collect_garbage` removes the rest (see there).

use crate::{Error, Result, Store, valid_label};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

/// Largest object: one workspace file (or manifest), output artifact or image.
pub const MAX_OBJECT: usize = 64 * 1024 * 1024;
/// Stored object bytes in all; garbage collection runs automatically near this.
pub const MAX_OBJECT_BYTES: i64 = 16 * 1024 * 1024 * 1024;
pub const MAX_STAGES: i64 = 1 << 20;
/// Staging pins. Workspace objects already pinned in a scope are shared, not re-pinned,
/// so pins grow with changed files.
pub const MAX_PINS: i64 = 1 << 22;
/// Distinct stored objects.
pub const MAX_OBJECTS: i64 = 1 << 20;
pub const MAX_RECEIPT_OBJECTS: usize = 8;

/// Immutable store identity, not a host path or a disclosure permission. The digest
/// uses spindle-object-v1 framing, not a raw-file SHA-256.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectReference {
    pub digest: String,
    pub byte_length: u64,
}

/// Versioned artifact receipt. Payload is opaque host data, not an image or grant.
/// All references in this envelope are checked by typed adoption; references hidden
/// inside the opaque payload remain the host's responsibility.
#[derive(Clone, Eq, PartialEq)]
pub struct ArtifactReceipt {
    pub objects: Vec<ObjectReference>,
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for ArtifactReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactReceipt")
            .field("objects", &self.objects.len())
            .field("payload_bytes", &self.payload.len())
            .finish_non_exhaustive()
    }
}

impl ArtifactReceipt {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.objects.len() > MAX_RECEIPT_OBJECTS {
            return Err(Error::Invalid);
        }
        let length = 9usize + self.objects.len() * 72;
        if self.payload.len() > crate::MAX_PAYLOAD.saturating_sub(length) {
            return Err(Error::Invalid);
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut bytes = b"SAR1".to_vec();
        bytes.push(self.objects.len() as u8);
        for object in &self.objects {
            if !valid_digest(&object.digest)
                || object.byte_length > MAX_OBJECT as u64
                || !seen.insert(&object.digest)
            {
                return Err(Error::Invalid);
            }
            bytes.extend_from_slice(object.digest.as_bytes());
            bytes.extend_from_slice(&object.byte_length.to_be_bytes());
        }
        bytes.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 9 || bytes.len() > crate::MAX_PAYLOAD || &bytes[..4] != b"SAR1" {
            return Err(Error::Invalid);
        }
        let count = bytes[4] as usize;
        if count > MAX_RECEIPT_OBJECTS {
            return Err(Error::Invalid);
        }
        let header = 5 + count * 72;
        let size = bytes.get(header..header + 4).ok_or(Error::Invalid)?;
        let size = u32::from_be_bytes(size.try_into().map_err(|_| Error::Invalid)?) as usize;
        if size != bytes.len() - header - 4 {
            return Err(Error::Invalid);
        }
        let mut objects = Vec::with_capacity(count);
        let mut seen = std::collections::BTreeSet::new();
        for item in bytes[5..header].chunks_exact(72) {
            let digest = std::str::from_utf8(&item[..64]).map_err(|_| Error::Invalid)?;
            let byte_length =
                u64::from_be_bytes(item[64..].try_into().map_err(|_| Error::Invalid)?);
            if !valid_digest(digest) || byte_length > MAX_OBJECT as u64 || !seen.insert(digest) {
                return Err(Error::Invalid);
            }
            objects.push(ObjectReference {
                digest: digest.to_owned(),
                byte_length,
            });
        }
        Ok(Self {
            objects,
            payload: bytes[header + 4..].to_vec(),
        })
    }
}

pub(crate) fn validate_references(
    db: &Connection,
    scope: &str,
    references: &[ObjectReference],
) -> Result<()> {
    for reference in references {
        reference_bytes(db, scope, reference)?;
    }
    Ok(())
}

pub(crate) fn reference_bytes(
    db: &Connection,
    scope: &str,
    reference: &ObjectReference,
) -> Result<Vec<u8>> {
    let bytes: Option<Vec<u8>> = db.query_row(
        "SELECT o.bytes FROM objects o WHERE o.digest=?1 AND EXISTS(SELECT 1 FROM staging_pins p JOIN artifact_stages s ON p.stage=s.id WHERE p.digest=o.digest AND s.scope=?2)",
        params![reference.digest, scope], |row| crate::bounded_blob(row, 0, MAX_OBJECT),
    ).optional()?;
    let bytes = bytes.ok_or(Error::Invalid)?;
    if object_digest(&bytes) != reference.digest {
        return Err(Error::Incompatible);
    }
    if bytes.len() as u64 != reference.byte_length {
        return Err(Error::Conflict);
    }
    Ok(bytes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stage {
    pub id: String,
    pub scope: String,
    pub owner_epoch: u64,
}

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE artifact_stages(id TEXT PRIMARY KEY, scope TEXT NOT NULL, owner_epoch INTEGER NOT NULL);
         CREATE TABLE objects(digest TEXT PRIMARY KEY, bytes BLOB NOT NULL);
         CREATE TABLE staging_pins(stage TEXT NOT NULL REFERENCES artifact_stages(id), digest TEXT NOT NULL REFERENCES objects(digest), PRIMARY KEY(stage,digest));
         CREATE TABLE image_provenance(scope TEXT NOT NULL, digest TEXT NOT NULL REFERENCES objects(digest), worker TEXT NOT NULL, source TEXT NOT NULL, width INTEGER NOT NULL, height INTEGER NOT NULL, owner_epoch INTEGER NOT NULL, PRIMARY KEY(scope,digest,worker,source));",
    )?;
    Ok(())
}

/// The digest a store keeps `bytes` under (a workspace file's `Entry::object`).
pub fn object_digest(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"spindle-object-v1\0");
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn validate_storage(db: &Connection) -> Result<()> {
    let (count, bytes): (i64, i64) = db.query_row(
        "SELECT count(*),coalesce(sum(length(bytes)),0) FROM objects",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let stages: i64 = db.query_row("SELECT count(*) FROM artifact_stages", [], |r| r.get(0))?;
    let pins: i64 = db.query_row("SELECT count(*) FROM staging_pins", [], |r| r.get(0))?;
    let provenance: i64 =
        db.query_row("SELECT count(*) FROM image_provenance", [], |r| r.get(0))?;
    let invalid: i64 = db.query_row(
        "SELECT (SELECT count(*) FROM objects WHERE length(bytes)>?1)
         + (SELECT count(*) FROM artifact_stages WHERE owner_epoch<=0 OR owner_epoch>(SELECT epoch FROM realm WHERE id=1) OR length(id) NOT BETWEEN 1 AND 128 OR length(scope) NOT BETWEEN 1 AND 128 OR NOT EXISTS(SELECT 1 FROM admissions a WHERE a.scope=artifact_stages.scope))
         + (SELECT count(*) FROM objects o WHERE NOT EXISTS(SELECT 1 FROM staging_pins p WHERE p.digest=o.digest))
         + (SELECT count(*) FROM image_provenance v WHERE length(v.digest)!=64 OR length(v.worker)!=64 OR length(v.source)!=64 OR v.width NOT BETWEEN 1 AND 1568 OR v.height NOT BETWEEN 1 AND 1568 OR v.owner_epoch<=0 OR v.owner_epoch>(SELECT epoch FROM realm WHERE id=1) OR NOT EXISTS(SELECT 1 FROM staging_pins p JOIN artifact_stages s ON p.stage=s.id WHERE p.digest=v.digest AND s.scope=v.scope))", [MAX_OBJECT as i64], |r| r.get(0),
    )?;
    if stages > MAX_STAGES
        || provenance > MAX_STAGES
        || count > MAX_OBJECTS
        || pins > MAX_PINS
        || bytes > MAX_OBJECT_BYTES
        || invalid != 0
    {
        return Err(Error::Incompatible);
    }
    let mut provenance_query =
        db.prepare("SELECT scope,digest,worker,source FROM image_provenance")?;
    let mut provenance_rows = provenance_query.query([])?;
    while let Some(row) = provenance_rows.next()? {
        let scope = crate::bounded_label(row, 0)?;
        for column in 1..4 {
            if !valid_digest(&crate::bounded_label(row, column)?) {
                return Err(Error::Incompatible);
            }
        }
        if !crate::valid_label(&scope) {
            return Err(Error::Incompatible);
        }
    }
    let mut query = db.prepare("SELECT digest,bytes FROM objects")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let expected: String = row.get(0)?;
        let bytes: Vec<u8> = crate::bounded_blob(row, 1, MAX_OBJECT)?;
        if !valid_digest(&expected) || object_digest(&bytes) != expected {
            return Err(Error::Incompatible);
        }
    }
    Ok(())
}

/// Stage, pin and object counts and stored object bytes. Kept per connection in TEMP
/// counters that triggers on the artifact tables maintain (rewinds insert directly),
/// so caps are checked without scanning; the first use counts once.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub stages: i64,
    pub pins: i64,
    pub objects: i64,
    pub bytes: i64,
}

/// Room one store write may still need, checked against the caps.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Room {
    pub stages: i64,
    pub pins: i64,
    pub objects: i64,
    pub bytes: i64,
}

impl Usage {
    fn fits(&self, room: Room) -> bool {
        self.stages + room.stages <= MAX_STAGES
            && self.pins + room.pins <= MAX_PINS
            && self.objects + room.objects <= MAX_OBJECTS
            && self.bytes + room.bytes <= MAX_OBJECT_BYTES
    }
    /// Above three quarters of any cap.
    #[cfg(feature = "driver")]
    fn crowded(&self) -> bool {
        self.stages * 4 > MAX_STAGES * 3
            || self.pins * 4 > MAX_PINS * 3
            || self.objects * 4 > MAX_OBJECTS * 3
            || self.bytes * 4 > MAX_OBJECT_BYTES * 3
    }
    /// Grown by a thirty-second of some cap since `earlier`.
    #[cfg(feature = "driver")]
    fn grown_since(&self, earlier: &Usage) -> bool {
        self.stages - earlier.stages > MAX_STAGES / 32
            || self.pins - earlier.pins > MAX_PINS / 32
            || self.objects - earlier.objects > MAX_OBJECTS / 32
            || self.bytes - earlier.bytes > MAX_OBJECT_BYTES / 32
    }
}

pub(crate) fn usage(db: &Connection) -> Result<Usage> {
    let counting: bool = db
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sqlite_temp_schema WHERE name='spindle_usage')",
        )?
        .query_row([], |r| r.get(0))?;
    if !counting {
        count_usage(db)?;
    }
    Ok(db
        .prepare_cached("SELECT stages,pins,objects,bytes FROM temp.spindle_usage WHERE id=1")?
        .query_row([], |r| {
            Ok(Usage {
                stages: r.get(0)?,
                pins: r.get(1)?,
                objects: r.get(2)?,
                bytes: r.get(3)?,
            })
        })?)
}

/// Counts once and installs the triggers that keep the counts from then on; done in
/// one batch, inside the caller's transaction when there is one.
fn count_usage(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS spindle_usage(id INTEGER PRIMARY KEY CHECK(id=1), stages INTEGER NOT NULL, pins INTEGER NOT NULL, objects INTEGER NOT NULL, bytes INTEGER NOT NULL);
         CREATE TEMP TRIGGER IF NOT EXISTS spindle_stage_added AFTER INSERT ON main.artifact_stages BEGIN UPDATE spindle_usage SET stages=stages+1; END;
         CREATE TEMP TRIGGER IF NOT EXISTS spindle_stage_removed AFTER DELETE ON main.artifact_stages BEGIN UPDATE spindle_usage SET stages=stages-1; END;
         CREATE TEMP TRIGGER IF NOT EXISTS spindle_pin_added AFTER INSERT ON main.staging_pins BEGIN UPDATE spindle_usage SET pins=pins+1; END;
         CREATE TEMP TRIGGER IF NOT EXISTS spindle_pin_removed AFTER DELETE ON main.staging_pins BEGIN UPDATE spindle_usage SET pins=pins-1; END;
         CREATE TEMP TRIGGER IF NOT EXISTS spindle_object_added AFTER INSERT ON main.objects BEGIN UPDATE spindle_usage SET objects=objects+1, bytes=bytes+length(NEW.bytes); END;
         CREATE TEMP TRIGGER IF NOT EXISTS spindle_object_removed AFTER DELETE ON main.objects BEGIN UPDATE spindle_usage SET objects=objects-1, bytes=bytes-length(OLD.bytes); END;
         INSERT OR IGNORE INTO spindle_usage SELECT 1,(SELECT count(*) FROM main.artifact_stages),(SELECT count(*) FROM main.staging_pins),(SELECT count(*) FROM main.objects),(SELECT coalesce(sum(length(bytes)),0) FROM main.objects);",
    )?;
    Ok(())
}

/// Pins `id` for `stage`, storing its bytes first unless already stored, within caps.
fn insert_pinned(
    tx: &Connection,
    stage: &Stage,
    id: &str,
    bytes: &[u8],
    stored: bool,
) -> Result<()> {
    let room = Room {
        pins: 1,
        objects: i64::from(!stored),
        bytes: if stored { 0 } else { bytes.len() as i64 },
        ..Room::default()
    };
    if !usage(tx)?.fits(room) {
        return Err(Error::Exhausted);
    }
    if !stored {
        crate::execute_one(tx, "INSERT INTO objects VALUES(?1,?2)", params![id, bytes])?;
    }
    crate::execute_one(
        tx,
        "INSERT INTO staging_pins VALUES(?1,?2)",
        params![stage.id, id],
    )
}

/// Child journal stages of run_code attempts that have a receipt: each stage's
/// `reserved` record names its parent and child id, which must derive the stage id.
/// Most nested calls one code-mode script makes (a ceiling policy may lower).
pub const MAX_CALLS: usize = 1024;

/// The stage holding a code-mode script's child call `id` (its journal and results).
#[cfg(feature = "workspace-tools")]
pub(crate) fn child_stage(parent: &str, id: u64) -> String {
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(format!("{parent}\0{id}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("code-{hash}")
}

#[cfg(feature = "workspace-tools")]
fn resolved_journals(db: &Connection) -> Result<Vec<(String, String, Option<String>)>> {
    let mut query = db.prepare(
        "SELECT s.id,s.scope,o.bytes FROM artifact_stages s JOIN staging_pins p ON p.stage=s.id JOIN objects o ON o.digest=p.digest WHERE substr(s.id,1,5)='code-'",
    )?;
    let rows = query.query_map([], |r| {
        Ok((
            crate::bounded_label(r, 0)?,
            crate::bounded_label(r, 1)?,
            crate::bounded_blob(r, 2, MAX_OBJECT)?,
        ))
    })?;
    let mut journals = Vec::new();
    let mut private = std::collections::BTreeSet::new();
    for row in rows {
        let (stage, scope, bytes) = row?;
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if record["kind"] == "result" && record.get("private").is_some_and(|value| !value.is_null())
        {
            private.insert(stage.clone());
        }
        let (Some(parent), Some(child)) =
            (record["parent"].as_str(), record["request"]["id"].as_u64())
        else {
            continue;
        };
        if record["kind"] != "reserved" || child_stage(parent, child) != stage {
            continue;
        }
        let resolved: bool = db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM receipts WHERE attempt=?1)")?
            .query_row([parent], |r| r.get(0))?;
        if resolved {
            journals.push((stage, scope, None));
        }
    }
    journals.retain(|(stage, _, _)| !private.contains(stage));
    Ok(journals)
}

/// The stage that keeps what a run_code attempt's nested model calls were billed once
/// its journal is collected; usage accounting reads it beside the journal.
#[cfg(feature = "workspace-tools")]
pub(crate) fn spend_stage(parent: &str) -> String {
    let hash: String = Sha256::digest(format!("spindle-spend-v1\0{parent}").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("spend-{hash}")
}

/// Before resolved journals are collected, each nested model call's outcome and usage
/// (never its prompt or reply text) is kept in its parent's spend stage, so the call is
/// still counted (once: by parent and child id) after its journal is gone.
#[cfg(feature = "workspace-tools")]
pub(crate) fn keep_spend(
    db: &Connection,
    epoch: i64,
    journals: &[(String, String, Option<String>)],
) -> Result<()> {
    use serde_json::{Value, json};
    for (stage, scope, _) in journals {
        let mut records: Vec<Value> = Vec::new();
        {
            let mut query = db.prepare_cached(
                "SELECT o.bytes FROM staging_pins p JOIN objects o ON o.digest=p.digest WHERE p.stage=?1",
            )?;
            let rows = query.query_map([stage], |r| crate::bounded_blob(r, 0, MAX_OBJECT))?;
            for bytes in rows {
                if let Ok(record) = serde_json::from_slice(&bytes?) {
                    records.push(record);
                }
            }
        }
        let Some(reserved) = records
            .iter()
            .find(|r| r["kind"] == "reserved" && r["request"]["method"] == "model")
        else {
            continue;
        };
        let Some(parent) = reserved["parent"].as_str() else {
            continue;
        };
        let response = records
            .iter()
            .find(|r| r["kind"] == "result")
            .map(|r| {
                let mut response = r["response"].clone();
                if let Some(data) = response.get_mut("data").and_then(Value::as_object_mut) {
                    data.remove("text");
                }
                response
            })
            .unwrap_or(Value::Null);
        let prepared = &reserved["prepared"];
        let bytes = serde_json::to_vec(&json!({"kind":"spend","parent":parent,"id":reserved["request"]["id"],
            "prepared":{"provider":prepared["provider"],"model":prepared["model"],"contract":prepared["contract"]},
            "response":response}))
        .map_err(|_| Error::Invalid)?;
        if bytes.len() > MAX_OBJECT {
            continue;
        }
        let digest = object_digest(&bytes);
        let spend = spend_stage(parent);
        db.prepare_cached("INSERT OR IGNORE INTO objects VALUES(?1,?2)")?
            .execute(params![digest, bytes])?;
        db.prepare_cached("INSERT OR IGNORE INTO artifact_stages VALUES(?1,?2,?3)")?
            .execute(params![spend, scope, epoch])?;
        db.prepare_cached("INSERT OR IGNORE INTO staging_pins VALUES(?1,?2)")?
            .execute(params![spend, digest])?;
    }
    Ok(())
}

/// What one garbage collection removed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Collected {
    pub stages: u64,
    pub pins: u64,
    pub objects: u64,
    pub bytes: u64,
}

impl Store {
    pub fn begin_stage(&mut self, stage: &Stage) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(&stage.id)
            || stage.id.starts_with("rewound-task-")
            || !valid_label(&stage.scope)
            || stage.owner_epoch != self.epoch
        {
            return Err(Error::Invalid);
        }
        let result = self.stage_transaction(stage);
        self.artifact_result(result)
    }

    fn stage_transaction(&mut self, stage: &Stage) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: i64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
        if epoch != self.epoch as i64 {
            return Err(Error::Stale);
        }
        crate::ownership::live_allowed(&tx, &stage.scope)?;
        let cancelled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
            [&stage.scope],
            |r| r.get(0),
        )?;
        if cancelled {
            return Err(Error::Cancelled);
        }
        let old: Option<(String, i64)> = tx
            .query_row(
                "SELECT scope,owner_epoch FROM artifact_stages WHERE id=?1",
                [&stage.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((scope, old_epoch)) = old {
            return if scope == stage.scope && old_epoch == stage.owner_epoch as i64 {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        let admitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM admissions WHERE scope=?1)",
            [&stage.scope],
            |r| r.get(0),
        )?;
        if !admitted {
            return Err(Error::Invalid);
        }
        if usage(&tx)?.stages >= MAX_STAGES {
            return Err(Error::Exhausted);
        }
        crate::execute_one(
            &tx,
            "INSERT INTO artifact_stages VALUES(?1,?2,?3)",
            params![stage.id, stage.scope, stage.owner_epoch as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Copies bounded caller bytes into the same transaction as their staging pin.
    pub fn put_object(&mut self, stage: &Stage, bytes: &[u8]) -> Result<String> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(&stage.id)
            || stage.id.starts_with("rewound-task-")
            || !valid_label(&stage.scope)
            || bytes.len() > MAX_OBJECT
        {
            return Err(Error::Invalid);
        }
        if stage.owner_epoch != self.epoch {
            return Err(Error::Stale);
        }
        self.put_object_checked(stage, bytes, false, |_, _| Ok(()))
    }

    /// As `put_object`, but bytes already pinned by any stage of the same scope are
    /// not pinned again. Only for objects reached through a workspace root, which is
    /// validated per scope; never for artifacts authorized through their own stage.
    pub fn put_workspace_object(&mut self, stage: &Stage, bytes: &[u8]) -> Result<String> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(&stage.id)
            || stage.id.starts_with("rewound-task-")
            || !valid_label(&stage.scope)
            || bytes.len() > MAX_OBJECT
        {
            return Err(Error::Invalid);
        }
        if stage.owner_epoch != self.epoch {
            return Err(Error::Stale);
        }
        let result = self.put_transaction(stage, bytes, false, true, |_, _| Ok(()));
        self.artifact_result(result)
    }

    pub(crate) fn put_object_checked<F>(
        &mut self,
        stage: &Stage,
        bytes: &[u8],
        allow_cancelled: bool,
        record: F,
    ) -> Result<String>
    where
        F: FnOnce(&Transaction<'_>, &str) -> Result<()>,
    {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(&stage.id)
            || stage.id.starts_with("rewound-task-")
            || !valid_label(&stage.scope)
            || bytes.len() > MAX_OBJECT
        {
            return Err(Error::Invalid);
        }
        if stage.owner_epoch != self.epoch {
            return Err(Error::Stale);
        }
        let result = self.put_transaction(stage, bytes, allow_cancelled, false, record);
        self.artifact_result(result)
    }

    fn put_transaction<F>(
        &mut self,
        stage: &Stage,
        bytes: &[u8],
        allow_cancelled: bool,
        share: bool,
        record: F,
    ) -> Result<String>
    where
        F: FnOnce(&Transaction<'_>, &str) -> Result<()>,
    {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: i64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
        if epoch != self.epoch as i64 {
            return Err(Error::Stale);
        }
        let bound: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM artifact_stages WHERE id=?1 AND scope=?2 AND owner_epoch=?3)", params![stage.id,stage.scope,stage.owner_epoch as i64], |r| r.get(0))?;
        if !bound {
            return Err(Error::Invalid);
        }
        let cancelled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
            [&stage.scope],
            |r| r.get(0),
        )?;
        if !allow_cancelled {
            crate::ownership::live_allowed(&tx, &stage.scope)?;
            if cancelled {
                return Err(Error::Cancelled);
            }
        }
        let id = object_digest(bytes);
        // Equal digests are equal bytes (SHA-256 over length-framed bytes), so a stored
        // object is never read back to compare.
        let stored: bool = tx
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)")?
            .query_row([&id], |r| r.get(0))?;
        let pinned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM staging_pins WHERE stage=?1 AND digest=?2)",
            params![stage.id, id],
            |r| r.get(0),
        )?;
        let shared = share
            && stored
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM staging_pins p JOIN artifact_stages s ON s.id=p.stage WHERE p.digest=?1 AND s.scope=?2)",
                params![id, stage.scope],
                |r| r.get(0),
            )?;
        if pinned || shared {
            record(&tx, &id)?;
            tx.commit()?;
            return Ok(id);
        }
        insert_pinned(&tx, stage, &id, bytes, stored)?;
        record(&tx, &id)?;
        tx.commit()?;
        Ok(id)
    }

    /// Stores changed workspace files of one run in a single transaction, as
    /// `put_workspace_object` would one by one. `read` supplies each file's bytes in
    /// turn, so only one is held at a time; its error (a file that could not be read)
    /// is returned as the inner `Err` with nothing stored.
    #[cfg(feature = "dext-strict-workspace")]
    pub(crate) fn put_workspace_files(
        &mut self,
        stage: &Stage,
        paths: &[String],
        read: &mut dyn FnMut(&str) -> std::io::Result<Vec<u8>>,
    ) -> Result<std::result::Result<Vec<String>, String>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(&stage.id)
            || stage.id.starts_with("rewound-task-")
            || !valid_label(&stage.scope)
        {
            return Err(Error::Invalid);
        }
        if stage.owner_epoch != self.epoch {
            return Err(Error::Stale);
        }
        let result = (|| -> Result<std::result::Result<Vec<String>, String>> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: i64 =
                tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
            if epoch != self.epoch as i64 {
                return Err(Error::Stale);
            }
            let bound: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM artifact_stages WHERE id=?1 AND scope=?2 AND owner_epoch=?3)", params![stage.id,stage.scope,stage.owner_epoch as i64], |r| r.get(0))?;
            if !bound {
                return Err(Error::Invalid);
            }
            crate::ownership::live_allowed(&tx, &stage.scope)?;
            let cancelled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
                [&stage.scope],
                |r| r.get(0),
            )?;
            if cancelled {
                return Err(Error::Cancelled);
            }
            let mut pinned = crate::workspace::scope_pins(&tx, &stage.scope)?;
            let mut digests = Vec::with_capacity(paths.len());
            for path in paths {
                let bytes = match read(path) {
                    Ok(bytes) if bytes.len() <= MAX_OBJECT => bytes,
                    Ok(_) => return Ok(Err(format!("`{path}` is larger than one stored object"))),
                    Err(error) => return Ok(Err(format!("`{path}` could not be read: {error}"))),
                };
                let id = object_digest(&bytes);
                if pinned.insert(id.clone()) {
                    let stored: bool = tx
                        .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)")?
                        .query_row([&id], |r| r.get(0))?;
                    insert_pinned(&tx, stage, &id, &bytes, stored)?;
                }
                digests.push(id);
            }
            tx.commit()?;
            Ok(Ok(digests))
        })();
        self.artifact_result(result)
    }

    /// Scope is an authenticated caller binding, not authentication by itself.
    pub fn read_object(&mut self, scope: &str, id: &str) -> Result<Vec<u8>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) || !valid_digest(id) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<Vec<u8>> {
            let bytes: Option<Vec<u8>> = self.db.query_row(
                "SELECT o.bytes FROM objects o WHERE o.digest=?1 AND EXISTS(SELECT 1 FROM staging_pins p JOIN artifact_stages s ON p.stage=s.id WHERE p.digest=o.digest AND s.scope=?2)",
                params![id,scope], |r| crate::bounded_blob(r, 0, MAX_OBJECT),
            ).optional()?;
            let bytes = bytes.ok_or(Error::Invalid)?;
            if bytes.len() > MAX_OBJECT || object_digest(&bytes) != id {
                return Err(Error::Incompatible);
            }
            Ok(bytes)
        })();
        self.artifact_result(result)
    }

    /// Storage used against the store's fixed caps, for `doctor`.
    pub fn capacity(&mut self) -> Result<Vec<(&'static str, i64, i64)>> {
        if self.paused {
            return Err(Error::Paused);
        }
        let count = |sql: &str| -> Result<i64> { Ok(self.db.query_row(sql, [], |r| r.get(0))?) };
        let revisions = count("SELECT revision FROM head WHERE id=1")?;
        let admissions = count("SELECT count(*) FROM admissions")?;
        let used = usage(&self.db);
        let used = self.artifact_result(used)?;
        Ok(vec![
            ("revisions", revisions, crate::MAX_ADMISSIONS),
            ("admissions", admissions, crate::MAX_ADMISSIONS),
            ("stages", used.stages, MAX_STAGES),
            ("pins", used.pins, MAX_PINS),
            ("objects", used.objects, MAX_OBJECTS),
            ("object bytes", used.bytes, MAX_OBJECT_BYTES),
        ])
    }

    /// Whether `room` fits. Near the caps garbage is collected first, but only again
    /// once usage has grown noticeably since the last collection on this connection
    /// (or when `room` would not fit), so a store full of reachable data is not
    /// rescanned on every call.
    #[cfg(feature = "driver")]
    pub(crate) fn make_room(&mut self, room: Room) -> Result<bool> {
        if self.paused {
            return Err(Error::Paused);
        }
        let current = usage(&self.db);
        let current = self.artifact_result(current)?;
        if current.fits(room) && !current.crowded() {
            return Ok(true);
        }
        let last: Option<Usage> = self
            .db
            .query_row(
                "SELECT stages,pins,objects,bytes FROM temp.spindle_collected WHERE id=1",
                [],
                |r| {
                    Ok(Usage {
                        stages: r.get(0)?,
                        pins: r.get(1)?,
                        objects: r.get(2)?,
                        bytes: r.get(3)?,
                    })
                },
            )
            .optional()
            .unwrap_or(None);
        let due = match &last {
            None => true,
            Some(last) if current.fits(room) => current.grown_since(last),
            Some(last) => current != *last,
        };
        if due {
            self.collect_garbage()?;
        }
        let after = usage(&self.db);
        Ok(self.artifact_result(after)?.fits(room))
    }

    /// Removes stored data nothing can reach any more. Kept: every object reachable
    /// from a workspace root that a revision, receipt, strict receipt or rewound task
    /// names (in the root's scope); every stage that is not a resolved strict tool
    /// attempt's own (host, image, import and rewind stages, and attempts still
    /// unresolved); the retained output artifact a strict or app tool receipt names, pinned by its
    /// attempt's stage so `read_output` keeps working; and the child journal of every
    /// unresolved run_code attempt (journals of resolved scripts, kept for inspection,
    /// are collected too once the store is half full). Objects whose last pin went
    /// are deleted. One transaction; space is reused by later writes, not returned to
    /// the filesystem.
    pub fn collect_garbage(&mut self) -> Result<Collected> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = self.collect_transaction();
        self.artifact_result(result)
    }

    #[cfg(not(feature = "workspace-tools"))]
    fn collect_transaction(&mut self) -> Result<Collected> {
        Ok(Collected::default())
    }

    #[cfg(feature = "workspace-tools")]
    fn collect_transaction(&mut self) -> Result<Collected> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: i64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
        if epoch != self.epoch as i64 {
            return Err(Error::Stale);
        }
        let before = usage(&tx)?;
        tx.execute_batch(
            "DROP TABLE IF EXISTS temp.gc_live; DROP TABLE IF EXISTS temp.gc_freed; DROP TABLE IF EXISTS temp.gc_pinned;
             CREATE TEMP TABLE gc_live(scope TEXT NOT NULL, digest TEXT NOT NULL, PRIMARY KEY(scope,digest));
             CREATE TEMP TABLE gc_freed(digest TEXT PRIMARY KEY);",
        )?;
        let mut roots: Vec<(String, String)> = Vec::new();
        {
            let mut query = tx.prepare(
                "SELECT scope,workspace FROM revisions WHERE workspace IS NOT NULL
                 UNION SELECT v.scope,c.workspace FROM receipts c JOIN attempts a ON a.id=c.attempt JOIN intents i ON i.id=a.effect JOIN revisions v ON v.revision=i.revision WHERE c.workspace IS NOT NULL
                 UNION SELECT v.scope,s.base FROM strict_receipts s JOIN attempts a ON a.id=s.attempt JOIN intents i ON i.id=a.effect JOIN revisions v ON v.revision=i.revision
                 UNION SELECT v.scope,s.root FROM strict_receipts s JOIN attempts a ON a.id=s.attempt JOIN intents i ON i.id=a.effect JOIN revisions v ON v.revision=i.revision
                 UNION SELECT scope,workspace FROM rewound_tasks
                 UNION SELECT source_scope,workspace FROM rewound_tasks
                 UNION SELECT 'operator',base FROM task_runs",
            )?;
            let rows =
                query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                roots.push(row?);
            }
        }
        for (scope, root) in &roots {
            let live = |digest: &str| -> Result<()> {
                tx.prepare_cached("INSERT OR IGNORE INTO temp.gc_live VALUES(?1,?2)")?
                    .execute(params![scope, digest])?;
                Ok(())
            };
            live(root)?;
            let manifest: Option<Vec<u8>> = tx
                .prepare_cached("SELECT bytes FROM objects WHERE digest=?1")?
                .query_row([root], |r| crate::bounded_blob(r, 0, MAX_OBJECT))
                .optional()?;
            // A root's manifest is checked whenever it is adopted; one that does not
            // decode means a damaged store, where nothing is collected (its files
            // could not be told live).
            let Some(tree) = manifest.and_then(|bytes| crate::workspace::decode_tree(&bytes).ok())
            else {
                return Err(Error::Incompatible);
            };
            for entry in &tree.files {
                live(&entry.object)?;
            }
        }
        // Resolved strict attempts' stages, with the output artifact their receipt keeps.
        let mut collectable: Vec<(String, String, Option<String>)> = Vec::new();
        {
            let mut query = tx.prepare(
                "SELECT s.id,s.scope,r.result FROM artifact_stages s JOIN attempts a ON a.id=s.id JOIN receipts r ON r.attempt=a.id WHERE substr(a.implementation,1,20)='dext-strict-bash-v1-' OR substr(a.implementation,1,18)='dext-host-bash-v1-' OR substr(a.implementation,1,20)='spindle-app-tool-v1-'",
            )?;
            let rows = query.query_map([], |r| {
                Ok((
                    crate::bounded_label(r, 0)?,
                    crate::bounded_label(r, 1)?,
                    crate::packed_blob(&tx, r, 2, crate::MAX_PAYLOAD)?,
                ))
            })?;
            for row in rows {
                let (stage, scope, result) = row?;
                let artifact = match serde_json::from_slice::<dext_transcript::Block>(&result) {
                    Ok(dext_transcript::Block::ToolResult { metadata, .. }) => metadata.artifact,
                    _ => None,
                };
                collectable.push((stage, scope, artifact));
            }
        }
        let crowded = before.stages * 2 > MAX_STAGES
            || before.pins * 2 > MAX_PINS
            || before.objects * 2 > MAX_OBJECTS
            || before.bytes * 2 > MAX_OBJECT_BYTES;
        if crowded {
            let journals = resolved_journals(&tx)?;
            keep_spend(&tx, epoch, &journals)?;
            collectable.extend(journals);
        }
        for (stage, scope, keep) in &collectable {
            let keep = keep.as_deref().unwrap_or("");
            tx.prepare_cached(
                "INSERT OR IGNORE INTO temp.gc_freed SELECT p.digest FROM staging_pins p WHERE p.stage=?1 AND p.digest!=?3 AND NOT EXISTS(SELECT 1 FROM temp.gc_live l WHERE l.scope=?2 AND l.digest=p.digest)",
            )?
            .execute(params![stage, scope, keep])?;
            tx.prepare_cached(
                "DELETE FROM staging_pins WHERE stage=?1 AND digest!=?3 AND NOT EXISTS(SELECT 1 FROM temp.gc_live l WHERE l.scope=?2 AND l.digest=staging_pins.digest)",
            )?
            .execute(params![stage, scope, keep])?;
            tx.prepare_cached(
                "DELETE FROM artifact_stages WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM staging_pins WHERE stage=?1)",
            )?
            .execute([stage])?;
        }
        tx.execute_batch(
            "CREATE TEMP TABLE gc_pinned AS SELECT DISTINCT digest FROM staging_pins;
             DELETE FROM objects WHERE digest IN (SELECT digest FROM temp.gc_freed EXCEPT SELECT digest FROM temp.gc_pinned);
             DROP TABLE temp.gc_live; DROP TABLE temp.gc_freed; DROP TABLE temp.gc_pinned;
             CREATE TEMP TABLE IF NOT EXISTS spindle_collected(id INTEGER PRIMARY KEY CHECK(id=1), stages INTEGER NOT NULL, pins INTEGER NOT NULL, objects INTEGER NOT NULL, bytes INTEGER NOT NULL);",
        )?;
        crate::workspace::forget_validated_roots(&tx)?;
        let after = usage(&tx)?;
        tx.execute(
            "INSERT OR REPLACE INTO temp.spindle_collected VALUES(1,?1,?2,?3,?4)",
            params![after.stages, after.pins, after.objects, after.bytes],
        )?;
        tx.commit()?;
        Ok(Collected {
            stages: (before.stages - after.stages).max(0) as u64,
            pins: (before.pins - after.pins).max(0) as u64,
            objects: (before.objects - after.objects).max(0) as u64,
            bytes: (before.bytes - after.bytes).max(0) as u64,
        })
    }

    fn artifact_result<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }
}
