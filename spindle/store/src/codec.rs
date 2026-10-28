//! Deterministic compression and deduplication for the large values a store keeps:
//! admitted commands, continuations, proposals, intent inputs and receipt results.
//!
//! Callers only ever see logical bytes; every comparison (idempotent retries, proposal
//! re-derivation, receipt bindings) stays on those, so correctness never depends on
//! the compressed form. The form is still a pure function of the input: one long-range
//! pass replaces repeats of 128 bytes or more at any distance with copies (a waiting
//! step holds its history twice: once as the transcript and once inside the request),
//! then fixed-level deflate (pure Rust, pinned version) handles the rest.
//!
//! Stored layout: empty for an empty value, `0x00 ++ bytes` when compression does not
//! help, or `0x01 ++ u32 BE logical length ++ raw deflate(tokens)`. Tokens are
//! `0x00 varint(len) bytes` (literal) and `0x01 varint(distance) varint(len)` (copy
//! from `distance` bytes back; overlapping copies repeat).
//!
//! Schema 16 (`store`/`load`): a value over 16 KiB is cut into content-defined chunks
//! (gear hash, 4–64 KiB, about 16 KiB apart) kept once each in `chunks`, keyed by the
//! SHA-256 of their bytes and packed as above; the row holds `0x02 ++ u64 BE logical
//! length ++ the chunk digests`. Consecutive steps repeat almost all of their history,
//! so each step stores only the chunks that changed. Every chunk read is checked
//! against its digest, once for as long as the process keeps the chunk (`Recent`): a
//! row read again must hold the very bytes that were checked.
use crate::{Error, Result};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{LazyLock, Mutex, PoisonError};

const RAW: u8 = 0;
const PACKED: u8 = 1;
const CHUNKED: u8 = 2;
/// Values up to this size stay whole in their row.
const INLINE: usize = 16 * 1024;
/// Content-defined chunk bounds; a cut is taken where the top `CUT_BITS` of the gear
/// hash are zero, about every 2^CUT_BITS bytes past the minimum.
const CHUNK_MIN: usize = 4 * 1024;
pub(crate) const CHUNK_MAX: usize = 64 * 1024;
const CUT_BITS: u32 = 13;
const LITERAL: u8 = 0;
const COPY: u8 = 1;
/// Indexed block length; repeats are found wherever one aligned block recurs.
const BLOCK: usize = 64;
/// Shorter repeats stay literal: deflate's own 32 KiB window covers nearby ones.
const MIN_COPY: usize = 128;
const LEVEL: u8 = 6;
const BASE: u64 = 0x0100_0000_01b3;
/// Most bytes the process keeps of recent chunks (theirs and their bookkeeping's).
const RECENT_BYTES: usize = 32 * 1024 * 1024;
/// What each kept chunk costs beyond its bytes (key, maps, vector headers).
const RECENT_ENTRY: usize = 192;
/// Most chunks found under one quick key (more, crafted to share one, are hashed).
const RECENT_PER_KEY: usize = 4;

/// Chunks the process stored or read lately, by digest: their logical bytes and the
/// packed form a chunk row holds. Each step of a turn stores and reads its whole
/// history again, nearly all of it in chunks it had a moment before: a read whose row
/// holds the packed bytes kept here takes the logical bytes kept with them (they were
/// checked against the digest when kept) instead of inflating and hashing them again,
/// and a store finds a chunk it holds by its bytes, not by hashing it. What is kept is
/// a pure function of the bytes (content-addressed), so it serves any store and any
/// thread; a row that differs, or is missing, still fails as it would. The least
/// recently used go.
#[derive(Default)]
struct Recent {
    chunks: HashMap<[u8; 32], Kept>,
    /// Digests by a quick key of their logical bytes (a key shared by other bytes only
    /// costs a comparison).
    by_bytes: HashMap<u64, Vec<[u8; 32]>>,
    bytes: usize,
    clock: u64,
}

struct Kept {
    logical: Vec<u8>,
    /// The packed form a row was seen to hold (None: a row found there and not read).
    packed: Option<Vec<u8>>,
    used: u64,
}

static RECENT: LazyLock<Mutex<Recent>> = LazyLock::new(Mutex::default);

/// `work` on the recent chunks (each change to them completes before anything that can
/// panic, so a poisoned lock still holds them whole).
fn recent<T>(work: impl FnOnce(&mut Recent) -> T) -> T {
    work(&mut RECENT.lock().unwrap_or_else(PoisonError::into_inner))
}

impl Recent {
    /// Appends a kept chunk's logical bytes to `out` when its row holds `packed`.
    fn read(&mut self, digest: &[u8], packed: &[u8], out: &mut Vec<u8>, bound: usize) -> bool {
        self.clock += 1;
        let Ok(key) = <[u8; 32]>::try_from(digest) else {
            return false;
        };
        match self.chunks.get_mut(&key) {
            Some(kept) if kept.packed.as_deref() == Some(packed) && kept.logical.len() <= bound => {
                kept.used = self.clock;
                out.extend_from_slice(&kept.logical);
                true
            }
            _ => false,
        }
    }

    /// The digest of `chunk`, when it is kept.
    fn find(&mut self, chunk: &[u8]) -> Option<[u8; 32]> {
        self.clock += 1;
        let digest = *self.by_bytes.get(&quick(chunk))?.iter().find(|digest| {
            self.chunks
                .get(*digest)
                .is_some_and(|kept| kept.logical == chunk)
        })?;
        self.chunks.get_mut(&digest)?.used = self.clock;
        Some(digest)
    }

    /// Keeps a chunk checked against its digest (and the packed form its row holds).
    fn keep(&mut self, digest: [u8; 32], logical: &[u8], packed: Option<Vec<u8>>) {
        self.clock += 1;
        if let Some(kept) = self.chunks.get_mut(&digest) {
            kept.used = self.clock;
            if let Some(packed) = packed
                && kept.packed.as_ref() != Some(&packed)
            {
                self.bytes = self.bytes - kept.packed.as_ref().map_or(0, Vec::len) + packed.len();
                kept.packed = Some(packed);
            }
        } else {
            let same = self.by_bytes.entry(quick(logical)).or_default();
            if same.len() >= RECENT_PER_KEY {
                return;
            }
            same.push(digest);
            self.bytes += RECENT_ENTRY + logical.len() + packed.as_ref().map_or(0, Vec::len);
            self.chunks.insert(
                digest,
                Kept {
                    logical: logical.to_vec(),
                    packed,
                    used: self.clock,
                },
            );
        }
        if self.bytes > RECENT_BYTES {
            self.shrink();
        }
    }

    /// Lets the least recently used chunks go, to three quarters of the bound.
    fn shrink(&mut self) {
        let mut ages: Vec<(u64, [u8; 32])> = self
            .chunks
            .iter()
            .map(|(digest, kept)| (kept.used, *digest))
            .collect();
        ages.sort_unstable();
        for (_, digest) in ages {
            if self.bytes <= RECENT_BYTES / 4 * 3 {
                break;
            }
            let Some(kept) = self.chunks.remove(&digest) else {
                continue;
            };
            self.bytes -=
                RECENT_ENTRY + kept.logical.len() + kept.packed.map_or(0, |packed| packed.len());
            let key = quick(&kept.logical);
            if let Some(digests) = self.by_bytes.get_mut(&key) {
                digests.retain(|other| *other != digest);
                if digests.is_empty() {
                    self.by_bytes.remove(&key);
                }
            }
        }
    }
}

/// A chunk's digest.
fn sha256(chunk: &[u8]) -> [u8; 32] {
    #[cfg(test)]
    HASHED.with(|hashed| hashed.set(hashed.get() + 1));
    Sha256::digest(chunk).into()
}

#[cfg(test)]
thread_local! {
    /// Chunks hashed by this thread (what `Recent` saves is counted by the tests).
    static HASHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A quick key of a chunk's bytes (not a digest: equal keys are compared in full).
fn quick(bytes: &[u8]) -> u64 {
    let mut hash = (bytes.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        let value = u64::from_le_bytes(word.try_into().unwrap_or_default());
        hash = (hash ^ value)
            .wrapping_mul(0xbf58_476d_1ce4_e5b9)
            .rotate_left(29);
    }
    for byte in words.remainder() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x94d0_49bb_1331_11eb);
    }
    hash
}

/// Largest stored form of a logical value bounded by `logical`.
pub const fn stored_bound(logical: usize) -> usize {
    logical + 1
}

pub fn pack(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    if bytes.len() >= 2 * BLOCK && bytes.len() <= u32::MAX as usize {
        let deflated = miniz_oxide::deflate::compress_to_vec(&tokens(bytes), LEVEL);
        if deflated.len() + 5 < bytes.len() + 1 {
            let mut out = Vec::with_capacity(deflated.len() + 5);
            out.push(PACKED);
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend(deflated);
            return out;
        }
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(RAW);
    out.extend_from_slice(bytes);
    out
}

/// Logical bytes of a stored value, refused (`Incompatible`) when malformed or larger
/// than `bound`. Decompression never allocates past the declared, bounded length.
pub fn unpack(stored: &[u8], bound: usize) -> Result<Vec<u8>> {
    let Some((&tag, rest)) = stored.split_first() else {
        return Ok(Vec::new());
    };
    match tag {
        RAW if !rest.is_empty() && rest.len() <= bound => Ok(rest.to_vec()),
        PACKED if rest.len() >= 4 => {
            let length =
                u32::from_be_bytes(rest[..4].try_into().map_err(|_| Error::Incompatible)?) as usize;
            if length == 0 || length > bound {
                return Err(Error::Incompatible);
            }
            let limit = length + length / 4 + 64;
            let tokens = miniz_oxide::inflate::decompress_to_vec_with_limit(&rest[4..], limit)
                .map_err(|_| Error::Incompatible)?;
            expand(&tokens, length)
        }
        _ => Err(Error::Incompatible),
    }
}

/// The key is already a hash of the block; hashing it again only costs time.
#[derive(Default)]
struct Fingerprint(u64);
impl Hasher for Fingerprint {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*byte);
        }
    }
    fn write_u64(&mut self, value: u64) {
        // The polynomial key's low bits see only the low bits of its bytes; mix them so
        // crafted input cannot crowd one bucket.
        self.0 = (value ^ (value >> 31)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

/// Fixed pseudo-random gear table (splitmix64), part of the stored format.
const GEAR: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state = 0x5350_4e44_4745_4152u64;
    let mut index = 0;
    while index < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[index] = value ^ (value >> 31);
        index += 1;
    }
    table
};

/// End offsets of the content-defined chunks of `bytes`.
fn boundaries(bytes: &[u8]) -> Vec<usize> {
    let mut ends = vec![];
    let mut start = 0;
    while start < bytes.len() {
        let limit = (start + CHUNK_MAX).min(bytes.len());
        let mut end = limit;
        let mut hash = 0u64;
        let mut index = start + CHUNK_MIN.min(limit - start);
        while index < limit {
            hash = (hash << 1).wrapping_add(GEAR[bytes[index] as usize]);
            index += 1;
            if hash >> (64 - CUT_BITS) == 0 {
                end = index;
                break;
            }
        }
        ends.push(end);
        start = end;
    }
    ends
}

/// The stored form of `bytes` (schema 16): small values inline, larger ones as a
/// chunk manifest whose chunks are added to `chunks` if not already there.
pub fn store(db: &Connection, bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() <= INLINE {
        return Ok(pack(bytes));
    }
    let mut out = Vec::with_capacity(9 + bytes.len() / 512);
    out.push(CHUNKED);
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    let mut start = 0;
    for end in boundaries(bytes) {
        let chunk = &bytes[start..end];
        let digest = recent(|recent| recent.find(chunk)).unwrap_or_else(|| sha256(chunk));
        let known: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM chunks WHERE digest=?1)",
            [&digest[..]],
            |row| row.get(0),
        )?;
        // A new row is packed here, whatever form another store keeps it in: rows stay a
        // function of their bytes.
        let mut packed = None;
        if !known {
            let stored = pack(chunk);
            crate::execute_one(
                db,
                "INSERT INTO chunks VALUES(?1,?2)",
                rusqlite::params![&digest[..], &stored],
            )?;
            packed = Some(stored);
        }
        recent(|recent| recent.keep(digest, chunk, packed));
        out.extend_from_slice(&digest);
        start = end;
    }
    Ok(out)
}

/// Checks one stored chunk against its digest (the full integrity check).
pub(crate) fn verify_chunk(digest: &[u8], packed: &[u8]) -> Result<()> {
    let chunk = unpack(packed, CHUNK_MAX)?;
    if chunk.is_empty() || Sha256::digest(&chunk)[..] != *digest {
        return Err(Error::Incompatible);
    }
    Ok(())
}

/// Logical bytes of a value in any schema-15/16 stored form, at most `bound` of them.
/// Chunks are read from `db` and must match their digests.
pub fn load(db: &Connection, stored: &[u8], bound: usize) -> Result<Vec<u8>> {
    if stored.first() != Some(&CHUNKED) {
        return unpack(stored, bound);
    }
    let rest = &stored[1..];
    if rest.len() < 8 + 32 || !(rest.len() - 8).is_multiple_of(32) {
        return Err(Error::Incompatible);
    }
    let length = u64::from_be_bytes(rest[..8].try_into().map_err(|_| Error::Incompatible)?);
    let length = usize::try_from(length).map_err(|_| Error::Incompatible)?;
    // Every chunk holds 1..=CHUNK_MAX bytes and all but the last at least CHUNK_MIN, so
    // the count bounds the length both ways before anything is allocated.
    let count = (rest.len() - 8) / 32;
    if length <= INLINE
        || length > bound
        || count > length / CHUNK_MIN + 1
        || length > count.saturating_mul(CHUNK_MAX)
    {
        return Err(Error::Incompatible);
    }
    let mut out = Vec::with_capacity(length);
    for digest in rest[8..].chunks_exact(32) {
        let packed: Vec<u8> = db
            .query_row(
                "SELECT bytes FROM chunks WHERE digest=?1",
                [digest],
                |row| crate::bounded_blob(row, 0, stored_bound(CHUNK_MAX)),
            )
            .optional()?
            .ok_or(Error::Incompatible)?;
        let bound = CHUNK_MAX.min(length - out.len());
        if recent(|recent| recent.read(digest, &packed, &mut out, bound)) {
            continue;
        }
        let chunk = unpack(&packed, bound)?;
        if chunk.is_empty() || sha256(&chunk)[..] != *digest {
            return Err(Error::Incompatible);
        }
        if let Ok(digest) = <[u8; 32]>::try_from(digest) {
            recent(|recent| recent.keep(digest, &chunk, Some(packed)));
        }
        out.extend_from_slice(&chunk);
    }
    if out.len() != length {
        return Err(Error::Incompatible);
    }
    Ok(out)
}

fn fingerprint(block: &[u8]) -> u64 {
    block.iter().fold(0u64, |hash, byte| {
        hash.wrapping_mul(BASE).wrapping_add(u64::from(*byte))
    })
}

fn varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn literal(out: &mut Vec<u8>, bytes: &[u8]) {
    if !bytes.is_empty() {
        out.push(LITERAL);
        varint(out, bytes.len());
        out.extend_from_slice(bytes);
    }
}

fn tokens(input: &[u8]) -> Vec<u8> {
    let n = input.len();
    let mut out = Vec::with_capacity(n + 16);
    // BASE^(BLOCK-1): the weight of the byte leaving the rolling window.
    let leaving = (1..BLOCK).fold(1u64, |power, _| power.wrapping_mul(BASE));
    let mut table: HashMap<u64, usize, BuildHasherDefault<Fingerprint>> = HashMap::default();
    let mut indexed = 0;
    let mut pending = 0;
    let mut i = 0;
    let mut hash = fingerprint(&input[..BLOCK]);
    while i + BLOCK <= n {
        // Only blocks wholly before `i` are candidates, so a match never reads ahead.
        while indexed + BLOCK <= i {
            table
                .entry(fingerprint(&input[indexed..indexed + BLOCK]))
                .or_insert(indexed);
            indexed += BLOCK;
        }
        if let Some(&j) = table.get(&hash)
            && input[j..j + BLOCK] == input[i..i + BLOCK]
        {
            let (mut start, mut from) = (i, j);
            while start > pending && from > 0 && input[start - 1] == input[from - 1] {
                start -= 1;
                from -= 1;
            }
            let mut end = i + BLOCK;
            while end < n && input[end] == input[end - (start - from)] {
                end += 1;
            }
            if end - start >= MIN_COPY {
                literal(&mut out, &input[pending..start]);
                out.push(COPY);
                varint(&mut out, start - from);
                varint(&mut out, end - start);
                pending = end;
                i = end;
                if i + BLOCK <= n {
                    hash = fingerprint(&input[i..i + BLOCK]);
                }
                continue;
            }
        }
        if i + BLOCK < n {
            hash = hash
                .wrapping_sub(u64::from(input[i]).wrapping_mul(leaving))
                .wrapping_mul(BASE)
                .wrapping_add(u64::from(input[i + BLOCK]));
        }
        i += 1;
    }
    literal(&mut out, &input[pending..]);
    out
}

fn expand(tokens: &[u8], length: usize) -> Result<Vec<u8>> {
    fn read(input: &mut &[u8]) -> Result<usize> {
        let mut value = 0usize;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = input.split_first().ok_or(Error::Incompatible)?;
            *input = rest;
            value |= usize::from(byte & 0x7f)
                .checked_shl(shift)
                .ok_or(Error::Incompatible)?;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::Incompatible)
    }
    // The declared length is not trusted for allocation before the tokens prove it.
    let mut out = Vec::with_capacity(length.min(INLINE));
    let mut input = tokens;
    while let Some((&tag, rest)) = input.split_first() {
        input = rest;
        let (distance, count) = match tag {
            LITERAL => {
                let count = read(&mut input)?;
                if count == 0 || count > input.len() || count > length - out.len() {
                    return Err(Error::Incompatible);
                }
                out.extend_from_slice(&input[..count]);
                input = &input[count..];
                continue;
            }
            COPY => (read(&mut input)?, read(&mut input)?),
            _ => return Err(Error::Incompatible),
        };
        if distance == 0 || distance > out.len() || count == 0 || count > length - out.len() {
            return Err(Error::Incompatible);
        }
        let start = out.len() - distance;
        if distance >= count {
            out.extend_from_within(start..start + count);
        } else {
            for index in 0..count {
                out.push(out[start + index]);
            }
        }
    }
    if out.len() != length {
        return Err(Error::Incompatible);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn round_trips_and_is_deterministic() {
        let history =
            br#"{"role":"user","content":[{"type":"text","text":"build the thing"}]}"#.repeat(900);
        let noise = sample(7, 70_000);
        let mut doubled = history.clone();
        doubled.extend_from_slice(&noise);
        doubled.extend_from_slice(&history);
        doubled.extend_from_slice(&noise[..40_000]);
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            b"x".to_vec(),
            vec![0; 127],
            vec![0; 129],
            vec![7; 300_000],
            sample(1, 5_000),
            sample(2, 300_000),
            history.clone(),
            doubled.clone(),
            [&noise[..], &noise[..]].concat(),
        ];
        for case in cases {
            let packed = pack(&case);
            assert_eq!(packed, pack(&case), "same bytes on every run");
            assert!(packed.len() <= stored_bound(case.len()));
            assert_eq!(unpack(&packed, case.len()).unwrap(), case);
            if !case.is_empty() {
                assert!(
                    unpack(&packed, case.len() - 1).is_err(),
                    "bound is enforced"
                );
            }
        }
        // A repeat far beyond deflate's 32 KiB window costs a copy, not a second body.
        let repeated = [&noise[..], &noise[..]].concat();
        assert!(pack(&repeated).len() < noise.len() + 1024);
        assert!(pack(&doubled).len() < pack(&history).len() + pack(&noise).len() + 2048);
    }

    fn chunk_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE chunks(digest BLOB PRIMARY KEY, bytes BLOB NOT NULL)")
            .unwrap();
        db
    }

    #[test]
    fn chunked_values_round_trip_and_share_unchanged_chunks() {
        let db = chunk_db();
        let history = sample(3, 600_000);
        let mut next = history.clone();
        next.splice(
            300_000..300_000,
            b"a new message in the middle".iter().copied(),
        );
        next.extend_from_slice(&sample(4, 20_000));
        let first = store(&db, &history).unwrap();
        let count = |db: &Connection| -> i64 {
            db.query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))
                .unwrap()
        };
        let before = count(&db);
        let second = store(&db, &next).unwrap();
        assert_eq!(
            first,
            store(&db, &history).unwrap(),
            "same bytes on every run"
        );
        assert_eq!(load(&db, &first, history.len()).unwrap(), history);
        assert_eq!(load(&db, &second, next.len()).unwrap(), next);
        // An insertion and an append cost a few new chunks, not a second copy.
        assert!(
            count(&db) - before <= 6,
            "{} new chunks",
            count(&db) - before
        );
        assert!(first.len() < 2_000);
        assert!(
            load(&db, &first, history.len() - 1).is_err(),
            "bound is enforced"
        );
        // A value just over the inline size may be a single chunk.
        let one = sample(5, INLINE + 100);
        let digest: [u8; 32] = Sha256::digest(&one).into();
        db.execute(
            "INSERT INTO chunks VALUES(?1,?2)",
            rusqlite::params![&digest[..], pack(&one)],
        )
        .unwrap();
        let mut single = vec![CHUNKED];
        single.extend_from_slice(&(one.len() as u64).to_be_bytes());
        single.extend_from_slice(&digest);
        assert_eq!(load(&db, &single, one.len()).unwrap(), one);
        // Small values stay inline and need no chunk table.
        assert_eq!(store(&db, b"small").unwrap(), pack(b"small"));
        // A damaged chunk or a missing one is refused, never returned.
        db.execute(
            "UPDATE chunks SET bytes=?1 WHERE rowid=1",
            [pack(b"tampered")],
        )
        .unwrap();
        assert!(load(&db, &first, history.len()).is_err());
        db.execute("DELETE FROM chunks", []).unwrap();
        assert!(load(&db, &second, next.len()).is_err());
        let mut forged = first.clone();
        forged[1..9].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(load(&db, &forged, usize::MAX).is_err());
    }

    #[test]
    fn recent_chunks_are_not_hashed_again_and_their_rows_still_count() {
        let hashed = || HASHED.with(std::cell::Cell::get);
        let rows = |db: &Connection| -> Vec<(Vec<u8>, Vec<u8>)> {
            let mut statement = db
                .prepare("SELECT digest, bytes FROM chunks ORDER BY digest")
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let db = chunk_db();
        let history = sample(6, 400_000);
        let first = store(&db, &history).unwrap();
        let chunks = (first.len() - 9) / 32;
        let mut next = history.clone();
        next.extend_from_slice(&sample(7, 20_000));
        let before = hashed();
        let second = store(&db, &next).unwrap();
        assert!(
            hashed() - before <= 3,
            "{} of {chunks} chunks hashed again",
            hashed() - before
        );
        let before = hashed();
        assert_eq!(load(&db, &second, next.len()).unwrap(), next);
        assert_eq!(hashed(), before, "chunks at hand are read without hashing");
        assert!(
            load(&db, &second, next.len() - 1).is_err(),
            "the bound holds for chunks at hand"
        );
        // Another store holds none of them: each is written there, packed there (rows are a
        // function of their bytes), and read back whole.
        let other = chunk_db();
        let copied = store(&other, &history).unwrap();
        assert_eq!(copied, first);
        let mine: std::collections::HashSet<_> = rows(&db).into_iter().collect();
        assert!(
            rows(&other).iter().all(|row| mine.contains(row)),
            "the same rows as packed in the first"
        );
        assert_eq!(load(&other, &copied, history.len()).unwrap(), history);
        // A row holding another valid form of its chunk reads (checked in full), and then
        // at hand in that form.
        let (digest, packed) = rows(&other).remove(0);
        let logical = unpack(&packed, CHUNK_MAX).unwrap();
        let another = if packed[0] == RAW {
            let mut deflated = vec![PACKED];
            deflated.extend_from_slice(&(logical.len() as u32).to_be_bytes());
            deflated.extend(miniz_oxide::deflate::compress_to_vec(
                &tokens(&logical),
                LEVEL,
            ));
            deflated
        } else {
            [&[RAW][..], &logical].concat()
        };
        assert_ne!(another, packed);
        other
            .execute(
                "UPDATE chunks SET bytes=?1 WHERE digest=?2",
                rusqlite::params![&another, &digest],
            )
            .unwrap();
        let before = hashed();
        assert_eq!(load(&other, &copied, history.len()).unwrap(), history);
        assert_eq!(hashed(), before + 1, "the row in another form was checked");
        assert_eq!(load(&other, &copied, history.len()).unwrap(), history);
        assert_eq!(hashed(), before + 1, "and then read as it is");
        // A row that changed, or went, fails though its chunk was at hand.
        other
            .execute(
                "UPDATE chunks SET bytes=?1 WHERE digest=?2",
                rusqlite::params![pack(b"tampered"), &digest],
            )
            .unwrap();
        assert!(load(&other, &copied, history.len()).is_err());
        db.execute("DELETE FROM chunks WHERE rowid=3", []).unwrap();
        assert!(load(&db, &second, next.len()).is_err());
        // Bounded: far more than it keeps passes through, and reads stay right.
        for seed in 0..48 {
            let value = sample(100 + seed, 600_000);
            let stored = store(&db, &value).unwrap();
            assert_eq!(load(&db, &stored, value.len()).unwrap(), value);
        }
        recent(|recent| {
            assert!(recent.bytes <= RECENT_BYTES, "{} bytes kept", recent.bytes);
            let counted: usize = recent
                .chunks
                .values()
                .map(|kept| {
                    RECENT_ENTRY + kept.logical.len() + kept.packed.as_ref().map_or(0, Vec::len)
                })
                .sum();
            assert_eq!(recent.bytes, counted, "what is kept is counted");
            assert_eq!(
                recent.by_bytes.values().map(Vec::len).sum::<usize>(),
                recent.chunks.len()
            );
        });
    }

    #[test]
    fn malformed_values_are_refused() {
        let good = pack(&vec![9u8; 4096]);
        assert!(unpack(&[RAW], 10).is_err());
        assert!(unpack(&[2, 0, 0], 10).is_err());
        assert!(unpack(&good[..good.len() - 1], 4096).is_err());
        let mut lying = good.clone();
        lying[1..5].copy_from_slice(&5000u32.to_be_bytes());
        assert!(unpack(&lying, 8192).is_err());
        // Copies may not reach before the start, and literals may not overrun.
        for tokens in [
            vec![COPY, 1, 4],
            vec![LITERAL, 2, b'a', COPY, 3, 1],
            vec![LITERAL, 9, b'a'],
            vec![LITERAL, 0],
            vec![9],
        ] {
            let mut stored = vec![PACKED];
            stored.extend_from_slice(&4u32.to_be_bytes());
            stored.extend(miniz_oxide::deflate::compress_to_vec(&tokens, LEVEL));
            assert!(unpack(&stored, 64).is_err(), "{tokens:?}");
        }
        // A bomb cannot allocate past the declared length.
        let mut bomb = vec![PACKED];
        bomb.extend_from_slice(&16u32.to_be_bytes());
        bomb.extend(miniz_oxide::deflate::compress_to_vec(
            &vec![0u8; 1 << 22],
            LEVEL,
        ));
        assert!(unpack(&bomb, 1 << 30).is_err());
    }
}
