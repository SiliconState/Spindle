//! Bounded same-realm scope ownership. History and process containment are separate.
use crate::{Admission, Command, Error, Result, Store, valid_label};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

pub const MAX_SCOPES: usize = 256;
pub const MAX_DEPTH: usize = 32;
pub const MAX_WAITS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeKind {
    Task,
    Service,
}
impl ScopeKind {
    fn code(self) -> i64 {
        match self {
            Self::Task => 0,
            Self::Service => 1,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeState {
    Active,
    Cancelling,
    Settled,
    /// No new admissions, joins, reservations or dispatch; in-flight receipts still
    /// record and adopt after resume. Never proof that live work has stopped.
    Suspended,
}
impl ScopeState {
    fn decode(value: u64) -> Result<Self> {
        match value {
            0 => Ok(Self::Active),
            1 => Ok(Self::Cancelling),
            2 => Ok(Self::Settled),
            3 => Ok(Self::Suspended),
            _ => Err(Error::Incompatible),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedScope {
    pub scope: String,
    pub parent: Option<String>,
    pub kind: ScopeKind,
    pub state: ScopeState,
    pub admission: u64,
}

pub const MAX_REPORT: usize = 256;

/// Takeover obligations for the host. `truncated` means more entries exist than
/// were returned; it is never evidence that the omitted work is resolved.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reconciliation {
    pub live_services: Vec<String>,
    /// Live services not re-confirmed in the current owner epoch. They and their
    /// subtrees get no new work until `confirm_service`; stopping them cancels the
    /// subtree instead.
    pub unconfirmed_services: Vec<String>,
    pub cancelling: Vec<String>,
    /// Admissions neither consumed nor explicitly dropped.
    pub queued: Vec<(String, u64)>,
    pub unresolved_attempts: Vec<String>,
    /// Attempts without receipts left in a finalized rewind's abandoned future. Their
    /// results can never be adopted or retried; the external outcome (for example
    /// provider billing) stays unknown.
    pub abandoned_attempts: Vec<String>,
    /// Attempts the provider refused before producing anything (recorded status and
    /// message), live or abandoned. Known outcomes, not unknown ones.
    pub rejected_attempts: Vec<String>,
    pub truncated: bool,
}

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE owned_scopes(scope TEXT PRIMARY KEY,parent TEXT REFERENCES owned_scopes(scope),kind INTEGER NOT NULL CHECK(kind IN (0,1)),state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 3),admission INTEGER NOT NULL UNIQUE REFERENCES admissions(sequence));
        CREATE TABLE owned_waits(waiter TEXT NOT NULL REFERENCES owned_scopes(scope),target TEXT NOT NULL REFERENCES owned_scopes(scope),PRIMARY KEY(waiter,target));
        CREATE TABLE service_confirmations(scope TEXT PRIMARY KEY REFERENCES owned_scopes(scope),epoch INTEGER NOT NULL);
        CREATE TABLE dropped_admissions(admission INTEGER PRIMARY KEY REFERENCES admissions(sequence),command TEXT NOT NULL,epoch INTEGER NOT NULL);")?;
    Ok(())
}

fn scopes(db: &Connection) -> Result<Vec<OwnedScope>> {
    let mut query = db
        .prepare("SELECT scope,parent,kind,state,admission FROM owned_scopes ORDER BY admission")?;
    let mut rows = query.query([])?;
    let mut result = vec![];
    while let Some(row) = rows.next()? {
        if result.len() >= MAX_SCOPES {
            return Err(Error::Incompatible);
        }
        result.push(OwnedScope {
            scope: crate::bounded_label(row, 0)?,
            parent: if row.get_ref(1)? == rusqlite::types::ValueRef::Null {
                None
            } else {
                Some(crate::bounded_label(row, 1)?)
            },
            kind: match crate::nonnegative_id(row, 2)? {
                0 => ScopeKind::Task,
                1 => ScopeKind::Service,
                _ => return Err(Error::Incompatible),
            },
            state: ScopeState::decode(crate::nonnegative_id(row, 3)?)?,
            admission: crate::nonnegative_id(row, 4)?,
        });
    }
    Ok(result)
}
#[cfg(feature = "dext-turn")]
pub(crate) fn scope_node(db: &Connection, scope: &str) -> Result<Option<OwnedScope>> {
    Ok(scopes(db)?.into_iter().find(|node| node.scope == scope))
}
fn ancestry<'a>(all: &'a [OwnedScope], scope: &str) -> Result<Vec<&'a OwnedScope>> {
    let mut result: Vec<&OwnedScope> = vec![];
    let mut current = all
        .iter()
        .find(|node| node.scope == scope)
        .ok_or(Error::Invalid)?;
    loop {
        if result.len() >= MAX_DEPTH || result.iter().any(|node| node.scope == current.scope) {
            return Err(Error::Incompatible);
        }
        result.push(current);
        let Some(parent) = &current.parent else {
            return Ok(result);
        };
        current = all
            .iter()
            .find(|node| &node.scope == parent)
            .ok_or(Error::Incompatible)?;
    }
}

#[cfg(feature = "dext-turn")]
pub(crate) fn cut_scopes(db: &Connection, scope: &str, admission: u64) -> Result<Vec<OwnedScope>> {
    let all = scopes(db)?;
    let Some(root) = all
        .iter()
        .find(|node| node.scope == scope && node.admission <= admission)
    else {
        return Err(Error::Invalid);
    };
    if root.parent.is_some() || root.kind != ScopeKind::Task {
        return Err(Error::Unsupported);
    }
    let mut included = vec![];
    for node in &all {
        if node.admission <= admission
            && ancestry(&all, &node.scope)?
                .iter()
                .any(|ancestor| ancestor.scope == scope)
        {
            included.push(node.clone());
        }
    }
    Ok(included)
}

pub(crate) fn descendants(db: &Connection, scope: &str) -> Result<Vec<String>> {
    let all = scopes(db)?;
    if !all.iter().any(|node| node.scope == scope) {
        return Ok(vec![scope.into()]);
    }
    let mut result: Vec<(usize, String)> = vec![];
    for node in &all {
        let path = ancestry(&all, &node.scope)?;
        if path.iter().any(|ancestor| ancestor.scope == scope)
            && (node.state != ScopeState::Settled || node.scope == scope)
        {
            result.push((path.len(), node.scope.clone()));
        }
    }
    result.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    Ok(result.into_iter().map(|(_, scope)| scope).collect())
}

/// New work: admissions, joins, reservations and adoption.
pub(crate) fn admit_allowed(db: &Connection, scope: &str) -> Result<()> {
    live_allowed(db, scope)?;
    let all = scopes(db)?;
    if all.iter().any(|node| node.scope == scope) {
        let path = ancestry(&all, scope)?;
        if path.iter().any(|node| node.state == ScopeState::Suspended) {
            return Err(Error::Suspended);
        }
        for node in path.iter().filter(|node| node.kind == ScopeKind::Service) {
            if !confirmed(db, &node.scope)? {
                return Err(Error::Unconfirmed);
            }
        }
    }
    Ok(())
}

/// Each takeover (owner epoch change) loses the old owner's service processes unless
/// the host verifies them again, so confirmation only counts for the current epoch.
/// A rewind also changes the epoch, which conservatively requires re-confirmation.
fn confirmed(db: &Connection, service: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM service_confirmations s JOIN realm r ON r.id=1 WHERE s.scope=?1 AND s.epoch=r.epoch)",
        [service],
        |row| row.get(0),
    )?)
}

/// Attempts without receipts whose intent is live or, with `live` false, was left in
/// a finalized rewind's abandoned future. Bounded to MAX_REPORT + 1 rows.
fn attempts_without_receipts(db: &Connection, live: bool) -> Result<Vec<String>> {
    let mut query = db.prepare(&format!(
        "SELECT a.id FROM attempts a JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE NOT EXISTS(SELECT 1 FROM receipts q WHERE q.attempt=a.id) AND NOT EXISTS(SELECT 1 FROM attempt_failures f WHERE f.attempt=a.id) AND {}({}) ORDER BY a.id LIMIT ?1",
        if live { "" } else { "NOT " },
        crate::rewind::LIVE_INTENT
    ))?;
    let mut rows = query.query([(MAX_REPORT + 1) as i64])?;
    let mut result = vec![];
    while let Some(row) = rows.next()? {
        result.push(crate::bounded_label(row, 0)?);
    }
    Ok(result)
}

/// Already-reserved work (execution, staging and result recording) drains under
/// suspension; refusing it would strand an attempt that can never be redispatched.
pub(crate) fn live_allowed(db: &Connection, scope: &str) -> Result<()> {
    crate::rewind::ensure_open(db)?;
    let all = scopes(db)?;
    if all.iter().any(|node| node.scope == scope)
        && ancestry(&all, scope)?
            .iter()
            .any(|node| matches!(node.state, ScopeState::Cancelling | ScopeState::Settled))
    {
        return Err(Error::Cancelled);
    }
    Ok(())
}

fn waits(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut query = db.prepare("SELECT waiter,target FROM owned_waits ORDER BY waiter,target")?;
    let mut rows = query.query([])?;
    let mut result = vec![];
    while let Some(row) = rows.next()? {
        if result.len() >= MAX_SCOPES * MAX_WAITS {
            return Err(Error::Incompatible);
        }
        result.push((crate::bounded_label(row, 0)?, crate::bounded_label(row, 1)?));
    }
    Ok(result)
}
fn cycle(edges: &[(String, String)], waiter: &str, target: &str) -> bool {
    let mut pending = vec![target];
    let mut visited = std::collections::BTreeSet::new();
    while let Some(node) = pending.pop() {
        if node == waiter {
            return true;
        }
        if visited.insert(node) {
            pending.extend(
                edges
                    .iter()
                    .filter(|(a, _)| a == node)
                    .map(|(_, b)| b.as_str()),
            );
        }
    }
    false
}

pub(crate) fn validate_storage(db: &Connection) -> Result<()> {
    let all = scopes(db)?;
    let edges = waits(db)?;
    for node in &all {
        let path = ancestry(&all, &node.scope)?;
        if node.kind == ScopeKind::Service && node.parent.is_none() {
            return Err(Error::Incompatible);
        }
        let admitted: String = db.query_row(
            "SELECT scope FROM admissions WHERE sequence=?1",
            [node.admission as i64],
            |row| crate::bounded_label(row, 0),
        )?;
        if admitted != node.scope {
            return Err(Error::Incompatible);
        }
        if let Some(parent) = path.get(1)
            && (parent.admission >= node.admission
                || (parent.state == ScopeState::Settled && node.state != ScopeState::Settled))
        {
            return Err(Error::Incompatible);
        }
        if path
            .iter()
            .any(|ancestor| ancestor.state == ScopeState::Cancelling)
            && matches!(node.state, ScopeState::Active | ScopeState::Suspended)
        {
            return Err(Error::Incompatible);
        }
        if path
            .iter()
            .any(|ancestor| ancestor.state == ScopeState::Suspended)
            && node.state == ScopeState::Active
        {
            return Err(Error::Incompatible);
        }
        let cancelled: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
            [&node.scope],
            |row| row.get(0),
        )?;
        if node.state == ScopeState::Cancelling && !cancelled {
            return Err(Error::Incompatible);
        }
        if matches!(node.state, ScopeState::Active | ScopeState::Suspended) && cancelled {
            return Err(Error::Incompatible);
        }
        if node.state == ScopeState::Settled && !settled_obligations(db, &all, node)? {
            return Err(Error::Incompatible);
        }
    }
    let mut dependencies = edges.clone();
    dependencies.extend(all.iter().filter_map(|node| {
        node.parent
            .as_ref()
            .map(|parent| (parent.clone(), node.scope.clone()))
    }));
    for (waiter, target) in &edges {
        let a = ancestry(&all, waiter)?;
        let b = ancestry(&all, target)?;
        if a.last().map(|n| &n.scope) != b.last().map(|n| &n.scope)
            || cycle(&dependencies, waiter, target)
        {
            return Err(Error::Incompatible);
        }
        if edges.iter().filter(|(node, _)| node == waiter).count() > MAX_WAITS {
            return Err(Error::Incompatible);
        }
    }
    let invalid: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM service_confirmations s LEFT JOIN owned_scopes o ON o.scope=s.scope WHERE o.kind IS NOT 1 OR s.epoch<=0 OR s.epoch>(SELECT epoch FROM realm))
         OR EXISTS(SELECT 1 FROM owned_scopes o WHERE o.kind=1 AND NOT EXISTS(SELECT 1 FROM service_confirmations s WHERE s.scope=o.scope))
         OR EXISTS(SELECT 1 FROM dropped_admissions d LEFT JOIN admissions a ON a.sequence=d.admission WHERE a.sequence IS NULL OR a.kind=?1 OR d.epoch<a.admitted_epoch OR d.epoch>(SELECT epoch FROM realm) OR EXISTS(SELECT 1 FROM consumed c WHERE c.admission=d.admission))",
        [crate::rewind::KIND],
        |row| row.get(0),
    )?;
    if invalid {
        return Err(Error::Incompatible);
    }
    Ok(())
}

fn settled_obligations(db: &Connection, all: &[OwnedScope], node: &OwnedScope) -> Result<bool> {
    if all.iter().any(|child| {
        child.parent.as_ref() == Some(&node.scope) && child.state != ScopeState::Settled
    }) {
        return Ok(false);
    }
    let wait_pending: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM owned_waits w JOIN owned_scopes t ON t.scope=w.target WHERE w.waiter=?1 AND t.state!=2)", [&node.scope], |row| row.get(0))?;
    if wait_pending {
        return Ok(false);
    }
    let cancelled: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
        [&node.scope],
        |row| row.get(0),
    )?;
    let pending: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM intents i JOIN revisions r ON r.revision=i.revision LEFT JOIN attempts a ON a.effect=i.id LEFT JOIN applied_receipts p ON p.attempt=a.id WHERE r.scope=?1 AND p.attempt IS NULL AND (?2=0 OR a.id IS NOT NULL))
         OR EXISTS(SELECT 1 FROM image_executions e JOIN attempts a ON a.id=e.attempt JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE r.scope=?1 AND e.state=0)",
        params![node.scope, cancelled], |row| row.get(0),
    )?;
    if pending {
        return Ok(false);
    }
    #[cfg(feature = "dext-turn")]
    if !cancelled {
        use dext_core::turn::Phase;
        use dext_transcript::turn::TurnContinuation;
        let latest: Option<(String, Vec<u8>)> = {
            use rusqlite::OptionalExtension;
            db.query_row("SELECT engine,continuation FROM revisions WHERE scope=?1 ORDER BY revision DESC LIMIT 1", [&node.scope], |row| Ok((crate::bounded_label(row, 0)?, crate::packed_blob(db, row, 1, crate::MAX_PAYLOAD)?))).optional()?
        };
        if let Some((engine, bytes)) = latest
            && engine == crate::turn::ENGINE
            && !matches!(
                TurnContinuation::restore(&bytes)
                    .map_err(|_| Error::Incompatible)?
                    .control()
                    .phase(),
                Phase::Stopped(_)
            )
        {
            return Ok(false);
        }
    }
    let unconsumed: bool = db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM admissions a WHERE a.scope=?1 AND a.kind!=?2 AND {})",
            crate::rewind::QUEUED
        ),
        params![node.scope, crate::rewind::KIND],
        |row| row.get(0),
    )?;
    Ok(cancelled || !unconsumed)
}

pub(crate) fn admit_owned_in_transaction(
    tx: &Transaction<'_>,
    epoch: u64,
    parent: Option<&str>,
    kind: ScopeKind,
    command: &Command,
) -> Result<Admission> {
    crate::validate(command)?;
    if (kind == ScopeKind::Service && parent.is_none())
        || parent.is_some_and(|value| !valid_label(value) || value == command.scope)
    {
        return Err(Error::Invalid);
    }
    let all = scopes(tx)?;
    if let Some(existing) = all.iter().find(|node| node.scope == command.scope) {
        if existing.parent.as_deref() != parent || existing.kind != kind {
            return Err(Error::Conflict);
        }
        let admission = crate::admit_in_transaction(tx, epoch, command)?;
        if admission.sequence != existing.admission {
            return Err(Error::Conflict);
        }
        return Ok(admission);
    }
    if all.len() >= MAX_SCOPES {
        return Err(Error::Exhausted);
    }
    let has_history: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM admissions WHERE scope=?1)",
        [&command.scope],
        |row| row.get(0),
    )?;
    if has_history {
        return Err(Error::Conflict);
    }
    if let Some(parent) = parent {
        admit_allowed(tx, parent)?;
        if ancestry(&all, parent)?.len() >= MAX_DEPTH {
            return Err(Error::Exhausted);
        }
    }
    let admission = crate::admit_in_transaction(tx, epoch, command)?;
    crate::execute_one(
        tx,
        "INSERT INTO owned_scopes VALUES(?1,?2,?3,0,?4)",
        params![
            command.scope,
            parent,
            kind.code(),
            admission.sequence as i64
        ],
    )?;
    if kind == ScopeKind::Service {
        crate::execute_one(
            tx,
            "INSERT INTO service_confirmations VALUES(?1,?2)",
            params![command.scope, epoch as i64],
        )?;
    }
    Ok(admission)
}

impl Store {
    /// Creation/admission/parent attachment commit together. Caller authenticates
    /// scope and placement; a background service must name its longer-lived owner.
    pub fn admit_owned(
        &mut self,
        parent: Option<&str>,
        kind: ScopeKind,
        command: &Command,
    ) -> Result<Admission> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let admission = admit_owned_in_transaction(&tx, self.epoch, parent, kind, command)?;
            tx.commit()?;
            Ok(admission)
        })();
        self.ownership_result(result)
    }

    pub fn owned_scopes(&mut self) -> Result<Vec<OwnedScope>> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = scopes(&self.db);
        self.ownership_result(result)
    }

    /// Waits cannot silently attach ownership or cross independent scope trees.
    pub fn join_owned(&mut self, waiter: &str, target: &str) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(waiter) || !valid_label(target) || waiter == target {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 = tx.query_row("SELECT epoch FROM realm", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            let all = scopes(&tx)?;
            let a = ancestry(&all, waiter)?;
            let b = ancestry(&all, target)?;
            if a.last().map(|node| &node.scope) != b.last().map(|node| &node.scope) {
                return Err(Error::Conflict);
            }
            let edges = waits(&tx)?;
            if edges.iter().any(|(w, t)| w == waiter && t == target) {
                return Ok(());
            }
            admit_allowed(&tx, waiter)?;
            let mut dependencies = edges.clone();
            dependencies.extend(all.iter().filter_map(|node| {
                node.parent
                    .as_ref()
                    .map(|parent| (parent.clone(), node.scope.clone()))
            }));
            if cycle(&dependencies, waiter, target) {
                return Err(Error::Conflict);
            }
            if edges.iter().filter(|(w, _)| w == waiter).count() >= MAX_WAITS {
                return Err(Error::Exhausted);
            }
            crate::execute_one(
                &tx,
                "INSERT INTO owned_waits VALUES(?1,?2)",
                params![waiter, target],
            )?;
            tx.commit()?;
            Ok(())
        })();
        self.ownership_result(result)
    }

    /// Settles only after children, joins and all operational obligations settle.
    /// No caller-supplied stopped Boolean can waive an unresolved effect.
    pub fn settle_owned(&mut self, scope: &str) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 = tx.query_row("SELECT epoch FROM realm", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            let all = scopes(&tx)?;
            let node = all
                .iter()
                .find(|node| node.scope == scope)
                .ok_or(Error::Invalid)?;
            if node.state == ScopeState::Settled {
                return Ok(());
            }
            if node.state == ScopeState::Suspended || !settled_obligations(&tx, &all, node)? {
                return Err(Error::Conflict);
            }
            crate::execute_one(
                &tx,
                "UPDATE owned_scopes SET state=2 WHERE scope=?1",
                [scope],
            )?;
            tx.commit()?;
            Ok(())
        })();
        self.ownership_result(result)
    }

    /// Suspends the scope and every active descendant atomically. Running work is
    /// not signalled; hosts stop or drain it and its receipts adopt after resume.
    pub fn suspend_owned(&mut self, scope: &str) -> Result<()> {
        self.set_suspension(scope, true)
    }

    /// Resumes the scope and its suspended descendants. Refused while any owner
    /// remains suspended, so a child cannot outrun its suspended parent.
    pub fn resume_owned(&mut self, scope: &str) -> Result<()> {
        self.set_suspension(scope, false)
    }

    fn set_suspension(&mut self, scope: &str, suspend: bool) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 = tx.query_row("SELECT epoch FROM realm", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            crate::rewind::ensure_open(&tx)?;
            let all = scopes(&tx)?;
            let path = ancestry(&all, scope)?;
            let (from, to, already) = if suspend {
                (0, 3, ScopeState::Suspended)
            } else {
                (3, 0, ScopeState::Active)
            };
            match path[0].state {
                state if state == already => return Ok(()),
                ScopeState::Active | ScopeState::Suspended => {}
                ScopeState::Cancelling | ScopeState::Settled => return Err(Error::Conflict),
            }
            if !suspend
                && path[1..]
                    .iter()
                    .any(|owner| owner.state != ScopeState::Active)
            {
                return Err(Error::Conflict);
            }
            for target in descendants(&tx, scope)? {
                tx.execute(
                    "UPDATE owned_scopes SET state=?2 WHERE scope=?1 AND state=?3",
                    params![target, to, from],
                )?;
            }
            validate_storage(&tx)?;
            tx.commit()?;
            Ok(())
        })();
        self.ownership_result(result)
    }

    /// Cancels every non-settled service owned directly by `owner` so the owner can
    /// settle after its foreground work. Each stop reuses idempotent cancel_scope, so
    /// a crash mid-way is completed by repeating the same call; nothing is restarted.
    pub fn stop_services(&mut self, owner: &str, command: &str) -> Result<Vec<String>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(owner) || !valid_label(command) || command.len() > 64 {
            return Err(Error::Invalid);
        }
        let all = self.ownership_result(scopes(&self.db))?;
        if !all.iter().any(|node| node.scope == owner) {
            return Err(Error::Invalid);
        }
        let services: Vec<String> = all
            .iter()
            .filter(|node| {
                node.parent.as_deref() == Some(owner)
                    && node.kind == ScopeKind::Service
                    && matches!(node.state, ScopeState::Active | ScopeState::Suspended)
            })
            .map(|node| node.scope.clone())
            .collect();
        for service in &services {
            self.cancel_scope(service, &format!("{command}/svc"))?;
        }
        Ok(services)
    }

    /// Read-only takeover report. Nothing here is resumed or retried automatically:
    /// live services had their processes lost with the old owner unless the host
    /// re-confirms them, queued admissions await processing or an explicit drop, and
    /// unresolved attempts may or may not have executed.
    pub fn reconciliation(&mut self) -> Result<Reconciliation> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = (|| -> Result<_> {
            let all = scopes(&self.db)?;
            let mut report = Reconciliation::default();
            for node in &all {
                match (node.kind, node.state) {
                    (ScopeKind::Service, ScopeState::Active | ScopeState::Suspended) => {
                        report.live_services.push(node.scope.clone());
                        if !confirmed(&self.db, &node.scope)? {
                            report.unconfirmed_services.push(node.scope.clone());
                        }
                    }
                    (_, ScopeState::Cancelling) => report.cancelling.push(node.scope.clone()),
                    _ => {}
                }
            }
            let mut query = self.db.prepare(&format!(
                "SELECT a.scope,a.sequence FROM admissions a WHERE a.kind!=?1 AND {} ORDER BY a.sequence LIMIT ?2",
                crate::rewind::QUEUED
            ))?;
            let mut rows = query.query(params![crate::rewind::KIND, (MAX_REPORT + 1) as i64])?;
            while let Some(row) = rows.next()? {
                report.queued.push((
                    crate::bounded_label(row, 0)?,
                    crate::nonnegative_id(row, 1)?,
                ));
            }
            report.unresolved_attempts = attempts_without_receipts(&self.db, true)?;
            report.abandoned_attempts = attempts_without_receipts(&self.db, false)?;
            let mut query = self
                .db
                .prepare("SELECT attempt FROM attempt_failures ORDER BY attempt LIMIT ?1")?;
            let mut rows = query.query([(MAX_REPORT + 1) as i64])?;
            while let Some(row) = rows.next()? {
                report.rejected_attempts.push(crate::bounded_label(row, 0)?);
            }
            report.truncated = report.queued.len() > MAX_REPORT
                || report.unresolved_attempts.len() > MAX_REPORT
                || report.abandoned_attempts.len() > MAX_REPORT
                || report.rejected_attempts.len() > MAX_REPORT;
            report.queued.truncate(MAX_REPORT);
            report.unresolved_attempts.truncate(MAX_REPORT);
            report.abandoned_attempts.truncate(MAX_REPORT);
            report.rejected_attempts.truncate(MAX_REPORT);
            Ok(report)
        })();
        self.ownership_result(result)
    }

    /// Host re-confirmation after takeover: the current owner verified this service's
    /// process and adopts it for this epoch. Never starts or restarts anything.
    pub fn confirm_service(&mut self, scope: &str) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 = tx.query_row("SELECT epoch FROM realm", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            crate::rewind::ensure_open(&tx)?;
            let all = scopes(&tx)?;
            let node = all
                .iter()
                .find(|node| node.scope == scope)
                .ok_or(Error::Invalid)?;
            if node.kind != ScopeKind::Service {
                return Err(Error::Invalid);
            }
            if !matches!(node.state, ScopeState::Active | ScopeState::Suspended) {
                return Err(Error::Conflict);
            }
            crate::execute_one(
                &tx,
                "UPDATE service_confirmations SET epoch=?2 WHERE scope=?1",
                params![scope, epoch as i64],
            )?;
            tx.commit()?;
            Ok(())
        })();
        self.ownership_result(result)
    }

    /// Settles a queued admission as dropped, so a new owner can decline work the old
    /// owner accepted. Idempotent per command. A dropped admission can never be
    /// consumed; resubmitting the same input needs a new command id.
    pub fn drop_admission(&mut self, sequence: u64, command: &str) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if sequence == 0 || sequence > i64::MAX as u64 || !valid_label(command) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            use rusqlite::OptionalExtension;
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 = tx.query_row("SELECT epoch FROM realm", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            crate::rewind::ensure_open(&tx)?;
            let kind: String = tx
                .query_row(
                    "SELECT kind FROM admissions WHERE sequence=?1",
                    [sequence as i64],
                    |row| crate::bounded_label(row, 0),
                )
                .optional()?
                .ok_or(Error::Invalid)?;
            if kind == crate::rewind::KIND {
                return Err(Error::Invalid);
            }
            let old: Option<String> = tx
                .query_row(
                    "SELECT command FROM dropped_admissions WHERE admission=?1",
                    [sequence as i64],
                    |row| crate::bounded_label(row, 0),
                )
                .optional()?;
            if let Some(old) = old {
                return if old == command {
                    Ok(())
                } else {
                    Err(Error::Conflict)
                };
            }
            let consumed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM consumed WHERE admission=?1)",
                [sequence as i64],
                |row| row.get(0),
            )?;
            if consumed {
                return Err(Error::Conflict);
            }
            crate::execute_one(
                &tx,
                "INSERT INTO dropped_admissions VALUES(?1,?2,?3)",
                params![sequence as i64, command, epoch as i64],
            )?;
            tx.commit()?;
            Ok(())
        })();
        self.ownership_result(result)
    }

    fn ownership_result<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }
}
