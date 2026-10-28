//! Trusted-host attempt reservations and receipt ingress. No native execution.
//! A reservation is not proof of approval or containment. Unknown attempts never
//! retry automatically: this initial adapter supports one attempt per effect.

use crate::{Error, MAX_PAYLOAD, Result, Store, valid_label};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

pub(crate) fn record_receipt(
    tx: &rusqlite::Transaction<'_>,
    attempt: &str,
    result: &[u8],
    workspace: Option<&str>,
) -> Result<()> {
    crate::fits(result.len())?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1)",
        [attempt],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(Error::Invalid);
    }
    // A refused attempt's outcome is already known: it can never also have a result.
    let refused: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM attempt_failures WHERE attempt=?1)",
        [attempt],
        |r| r.get(0),
    )?;
    if refused {
        return Err(Error::Conflict);
    }
    let old: Option<(Vec<u8>, Option<String>)> = tx
        .query_row(
            "SELECT result,workspace FROM receipts WHERE attempt=?1",
            [attempt],
            |r| {
                Ok((
                    crate::packed_blob(tx, r, 0, MAX_PAYLOAD)?,
                    crate::optional_digest(r, 1)?,
                ))
            },
        )
        .optional()?;
    if let Some((old, root)) = old {
        return if old == result && root.as_deref() == workspace {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    if let Some(root) = workspace {
        let scope: String = tx.query_row("SELECT r.scope FROM attempts a JOIN intents i ON a.effect=i.id JOIN revisions r ON i.revision=r.revision WHERE a.id=?1", [attempt], |r| r.get(0))?;
        crate::workspace::validate_root(tx, &scope, root)?;
    }
    crate::execute_one(
        tx,
        "INSERT INTO receipts VALUES(?1,?2,?3)",
        params![attempt, crate::codec::store(tx, result)?, workspace],
    )?;
    Ok(())
}

/// Durable root-worker evidence, independent of effect receipt/adoption. Neither
/// settled state resolves an external effect or permits an attempt to run again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageCleanup {
    Unknown,
    NotStarted,
    RootReaped,
    /// Root reaped and its owned cgroup reported empty by the kernel after kill.
    TreeEmptied,
}
impl ImageCleanup {
    pub(crate) fn decode(value: u64) -> Result<Self> {
        match value {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::NotStarted),
            2 => Ok(Self::RootReaped),
            3 => Ok(Self::TreeEmptied),
            _ => Err(Error::Incompatible),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attempt {
    pub id: String,
    pub effect: String,
    pub owner_epoch: u64,
    pub implementation: String,
    pub grant_reference: String,
}

impl Store {
    /// Realm-authority inspection only. Absence or Unknown is never evidence that
    /// an executor stopped. Settled cleanup does not resolve its effect receipt.
    pub fn image_cleanup(&mut self, scope: &str, attempt: &str) -> Result<Option<ImageCleanup>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) || !valid_label(attempt) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let state: Option<u64> = self.db.query_row(
                "SELECT e.state FROM image_executions e JOIN attempts a ON a.id=e.attempt JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE e.attempt=?1 AND r.scope=?2",
                params![attempt, scope], |row| crate::nonnegative_id(row, 0),
            ).optional()?;
            state.map(ImageCleanup::decode).transpose()
        })();
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }

    /// Permanently closes this experimental scope to new work. It does not stop
    /// processes or undo effects by itself. The controlled native-image broker
    /// commits through this method before signalling its worker. Receipt ingestion
    /// remains available for history.
    pub fn cancel_scope(&mut self, scope: &str, command_id: &str) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) || !valid_label(command_id) {
            return Err(Error::Invalid);
        }
        let outcome = self.cancel_transaction(scope, command_id);
        if matches!(
            outcome,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        outcome
    }

    fn cancel_transaction(&mut self, scope: &str, command_id: &str) -> Result<u64> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: i64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
        if epoch != self.epoch as i64 {
            return Err(Error::Stale);
        }
        let old: Option<(String, u64)> = tx
            .query_row(
                "SELECT command_id,epoch FROM cancellations WHERE scope=?1",
                [scope],
                |r| Ok((crate::bounded_label(r, 0)?, crate::nonnegative_id(r, 1)?)),
            )
            .optional()?;
        if let Some((old_id, old_epoch)) = old {
            if old_epoch == 0 || old_epoch > self.epoch {
                return Err(Error::Incompatible);
            }
            return if old_id == command_id {
                Ok(old_epoch)
            } else {
                Err(Error::Conflict)
            };
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM admissions WHERE scope=?1)",
            [scope],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::Invalid);
        }
        for target in crate::ownership::descendants(&tx, scope)? {
            let old: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
                [&target],
                |row| row.get(0),
            )?;
            if !old {
                crate::execute_one(
                    &tx,
                    "INSERT INTO cancellations VALUES(?1,?2,?3)",
                    params![target, command_id, epoch],
                )?;
            }
            let active: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM owned_scopes WHERE scope=?1 AND state IN (0,3))",
                [&target],
                |row| row.get(0),
            )?;
            if active {
                crate::execute_one(
                    &tx,
                    "UPDATE owned_scopes SET state=1 WHERE scope=?1 AND state IN (0,3)",
                    [&target],
                )?;
            }
        }
        tx.commit()?;
        Ok(self.epoch)
    }

    pub fn reserve_attempt(&mut self, attempt: &Attempt) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(&attempt.id)
            || !valid_label(&attempt.effect)
            || !valid_label(&attempt.implementation)
            || !valid_label(&attempt.grant_reference)
            || attempt.owner_epoch == 0
            || attempt.owner_epoch > i64::MAX as u64
        {
            return Err(Error::Invalid);
        }
        let result = self.reserve_transaction(attempt);
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }

    fn reserve_transaction(&mut self, attempt: &Attempt) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: i64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
        if epoch != self.epoch as i64 || attempt.owner_epoch != self.epoch {
            return Err(Error::Stale);
        }
        let cancelled: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM intents i JOIN revisions r ON i.revision=r.revision JOIN cancellations c ON c.scope=r.scope WHERE i.id=?1)", [&attempt.effect], |r| r.get(0))?;
        if cancelled {
            return Err(Error::Cancelled);
        }
        // Before the idempotent-retry branch, so no reservation for an intent that a
        // finalized rewind abandoned is accepted, whether new or a retry.
        if crate::rewind::superseded(&tx, &attempt.effect)? {
            return Err(Error::Conflict);
        }
        let existing: Option<(String, i64, String, String)> = tx.query_row(
            "SELECT effect,owner_epoch,implementation,grant_reference FROM attempts WHERE id=?1",
            [&attempt.id], |r| Ok((crate::bounded_label(r, 0)?,r.get(1)?,crate::bounded_label(r, 2)?,crate::bounded_label(r, 3)?)),
        ).optional()?;
        if let Some((effect, epoch, implementation, grant)) = existing {
            return if effect == attempt.effect
                && epoch == attempt.owner_epoch as i64
                && implementation == attempt.implementation
                && grant == attempt.grant_reference
            {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        let intent: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM intents WHERE id=?1)",
            [&attempt.effect],
            |r| r.get(0),
        )?;
        if !intent {
            return Err(Error::Invalid);
        }
        let scope: String = tx.query_row(
            "SELECT r.scope FROM intents i JOIN revisions r ON r.revision=i.revision WHERE i.id=?1",
            [&attempt.effect],
            |row| crate::bounded_label(row, 0),
        )?;
        crate::ownership::admit_allowed(&tx, &scope)?;
        let reserved: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE effect=?1)",
            [&attempt.effect],
            |r| r.get(0),
        )?;
        if reserved {
            return Err(Error::Conflict);
        }
        crate::execute_one(
            &tx,
            "INSERT INTO attempts VALUES(?1,?2,?3,?4,?5)",
            params![
                attempt.id,
                attempt.effect,
                attempt.owner_epoch as i64,
                attempt.implementation,
                attempt.grant_reference
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records that the provider refused `attempt` before producing anything: its HTTP
    /// status and the start of what it said. Only for a reserved attempt without a
    /// receipt; the attempt stays without one (a retry is a new request), but its
    /// outcome is known rather than unknown.
    pub fn record_rejection(&mut self, attempt: &str, status: u16, detail: &str) -> Result<()> {
        self.record_rejection_with_delay(attempt, status, detail, None)
    }

    pub fn record_rejection_with_delay(
        &mut self,
        attempt: &str,
        status: u16,
        detail: &str,
        retry_after: Option<std::time::Duration>,
    ) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(attempt) || !(100..=599).contains(&status) {
            return Err(Error::Invalid);
        }
        let mut end = detail.len().min(4096);
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        let result = (|| -> Result<()> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let open: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM attempts a WHERE a.id=?1 AND NOT EXISTS(SELECT 1 FROM receipts q WHERE q.attempt=a.id))",
                [attempt],
                |r| r.get(0),
            )?;
            if !open {
                return Err(Error::Conflict);
            }
            tx.execute(
                "INSERT OR REPLACE INTO attempt_failures VALUES(?1,?2,?3)",
                params![attempt, status, &detail[..end]],
            )?;
            if let Some(delay) = retry_after {
                let millis = i64::try_from(delay.as_millis()).map_err(|_| Error::Invalid)?;
                tx.execute("INSERT INTO retry_delays VALUES(?1,?2) ON CONFLICT(attempt) DO UPDATE SET millis=excluded.millis", params![attempt, millis])?;
            } else {
                tx.execute("DELETE FROM retry_delays WHERE attempt=?1", [attempt])?;
            }
            tx.commit()?;
            Ok(())
        })();
        if matches!(result, Err(Error::Sql(_) | Error::Io(_) | Error::Locked)) {
            self.paused = true;
        }
        result
    }

    pub fn retry_delay(&self, attempt: &str) -> Result<Option<std::time::Duration>> {
        if self.paused {
            return Err(Error::Paused);
        }
        let millis: Option<u64> = self
            .db
            .query_row(
                "SELECT millis FROM retry_delays WHERE attempt=?1",
                [attempt],
                |row| crate::nonnegative_id(row, 0),
            )
            .optional()?;
        Ok(millis.map(std::time::Duration::from_millis))
    }

    /// The recorded refusal of `attempt`, if the provider refused it.
    pub fn rejection(&mut self, attempt: &str) -> Result<Option<(u16, String)>> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = self
            .db
            .query_row(
                "SELECT status,detail FROM attempt_failures WHERE attempt=?1",
                [attempt],
                |r| Ok((r.get::<_, u16>(0)?, r.get(1)?)),
            )
            .optional();
        result.map_err(|error| {
            self.paused = true;
            error.into()
        })
    }

    /// The trusted broker supplies a sealed result; this function does not validate
    /// artifacts, authenticate a worker or establish that its writers stopped.
    /// Old-epoch results remain historical facts, not fresh dispatch permissions.
    pub fn ingest_receipt(&mut self, attempt: &str, result: &[u8]) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(attempt) {
            return Err(Error::Invalid);
        }
        crate::fits(result.len())?;
        self.ingest_workspace_receipt(attempt, result, None)
    }

    /// Binds the result to an immutable candidate root. Native sealing remains a
    /// trusted broker obligation; root existence alone never proves execution.
    pub fn ingest_workspace_receipt(
        &mut self,
        attempt: &str,
        result: &[u8],
        workspace: Option<&str>,
    ) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(attempt) {
            return Err(Error::Invalid);
        }
        crate::fits(result.len())?;
        let outcome = self.ingest_transaction(attempt, result, workspace);
        if matches!(
            outcome,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        outcome
    }

    fn ingest_transaction(
        &mut self,
        attempt: &str,
        result: &[u8],
        workspace: Option<&str>,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: i64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
        if epoch != self.epoch as i64 {
            return Err(Error::Stale);
        }
        record_receipt(&tx, attempt, result, workspace)?;
        tx.commit()?;
        Ok(())
    }

    /// Realm-authority inspection, not a tenant API or publication acknowledgement.
    pub fn workspace_receipt(&mut self, attempt: &str) -> Result<Option<String>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(attempt) {
            return Err(Error::Invalid);
        }
        let result: std::result::Result<Option<Option<String>>, _> = self
            .db
            .query_row(
                "SELECT workspace FROM receipts WHERE attempt=?1",
                [attempt],
                |row| crate::optional_digest(row, 0),
            )
            .optional();
        match result {
            Ok(root) => Ok(root.flatten()),
            Err(error) => {
                self.paused = true;
                Err(error.into())
            }
        }
    }

    /// The one attempt reserved for an effect, if any, with its original owner epoch.
    pub fn attempt_for(&mut self, effect: &str) -> Result<Option<Attempt>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(effect) {
            return Err(Error::Invalid);
        }
        let result = self
            .db
            .query_row(
                "SELECT id,owner_epoch,implementation,grant_reference FROM attempts WHERE effect=?1",
                [effect],
                |r| {
                    Ok(Attempt {
                        id: crate::bounded_label(r, 0)?,
                        effect: effect.into(),
                        owner_epoch: crate::nonnegative_id(r, 1)?,
                        implementation: crate::bounded_label(r, 2)?,
                        grant_reference: crate::bounded_label(r, 3)?,
                    })
                },
            )
            .optional();
        result.map_err(|error| {
            self.paused = true;
            error.into()
        })
    }

    pub fn receipt(&mut self, attempt: &str) -> Result<Option<Vec<u8>>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(attempt) {
            return Err(Error::Invalid);
        }
        let result = self
            .db
            .query_row(
                "SELECT result FROM receipts WHERE attempt=?1",
                [attempt],
                |r| crate::packed_blob(&self.db, r, 0, MAX_PAYLOAD),
            )
            .optional();
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.paused = true;
                Err(error.into())
            }
        }
    }
}
