//! Optional Dext sanitized-JPEG receipt profile. Framing checks do not decode pixels
//! or prove trusted sanitization. No paths, credentials or disclosure grants persist.
use crate::artifact::{ArtifactReceipt, MAX_OBJECT, MAX_RECEIPT_OBJECTS, ObjectReference};
use crate::{Error, Result};
use dext_core::image::{LONGEST_SIDE_CAP, sanitized_jpeg_matches_dimensions};
use rusqlite::Connection;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JpegDescriptor {
    pub object: ObjectReference,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ImageReceipt {
    pub images: Vec<JpegDescriptor>,
}

impl std::fmt::Debug for ImageReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageReceipt")
            .field("images", &self.images.len())
            .finish_non_exhaustive()
    }
}

fn valid_dimensions(image: &JpegDescriptor) -> bool {
    image.width > 0
        && image.height > 0
        && image.width.max(image.height) <= LONGEST_SIDE_CAP
        && image.object.byte_length > 0
        && image.object.byte_length <= MAX_OBJECT as u64
}

impl ImageReceipt {
    /// SAR1 references paired in order with SIJ1 profile-1 (sanitized JPEG) metadata.
    /// This declares the profile; trusted worker provenance is still host-owned.
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.images.is_empty()
            || self.images.len() > MAX_RECEIPT_OBJECTS
            || self.images.iter().any(|image| !valid_dimensions(image))
        {
            return Err(Error::Invalid);
        }
        let mut payload = b"SIJ1".to_vec();
        for image in &self.images {
            payload.push(1);
            payload.extend_from_slice(&image.width.to_be_bytes());
            payload.extend_from_slice(&image.height.to_be_bytes());
        }
        ArtifactReceipt {
            objects: self.objects(),
            payload,
        }
        .encode()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let receipt = ArtifactReceipt::decode(bytes)?;
        if receipt.objects.is_empty()
            || receipt.payload.len() != 4 + receipt.objects.len() * 9
            || !receipt.payload.starts_with(b"SIJ1")
        {
            return Err(Error::Invalid);
        }
        let mut images = Vec::with_capacity(receipt.objects.len());
        for (object, metadata) in receipt
            .objects
            .into_iter()
            .zip(receipt.payload[4..].chunks_exact(9))
        {
            if metadata[0] != 1 {
                return Err(Error::Unsupported);
            }
            let image = JpegDescriptor {
                object,
                width: u32::from_be_bytes(metadata[1..5].try_into().map_err(|_| Error::Invalid)?),
                height: u32::from_be_bytes(metadata[5..9].try_into().map_err(|_| Error::Invalid)?),
            };
            if !valid_dimensions(&image) {
                return Err(Error::Invalid);
            }
            images.push(image);
        }
        Ok(Self { images })
    }

    #[cfg(feature = "dext-image-continuation")]
    pub fn result_blocks(&self, scope: &str, call_id: &str) -> Result<Vec<dext_transcript::Block>> {
        use dext_transcript::{ArtifactImage, Block, ToolResultMetadata};
        if !crate::valid_label(scope) || call_id.trim().is_empty() || call_id.len() > 4096 {
            return Err(Error::Invalid);
        }
        // Validate metadata even when a host constructed the receipt in memory.
        self.encode()?;
        let mut blocks = vec![Block::ToolResult {
            tool_use_id: call_id.into(),
            content: format!(
                "Retained {} sanitized image artifact(s); provider disclosure requires current approval.",
                self.images.len()
            ),
            is_error: Some(false),
            metadata: ToolResultMetadata {
                status: Some("ok".into()),
                ..Default::default()
            },
        }];
        for image in &self.images {
            blocks.push(Block::ImageArtifact {
                reference: ArtifactImage {
                    scope: scope.into(),
                    digest: image.object.digest.clone(),
                    byte_length: image.object.byte_length,
                    width: image.width,
                    height: image.height,
                },
            });
        }
        Ok(blocks)
    }

    pub(crate) fn objects(&self) -> Vec<ObjectReference> {
        self.images
            .iter()
            .map(|image| image.object.clone())
            .collect()
    }
}

fn hex_digest(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(feature = "dext-native-image")]
impl crate::Store {
    /// Only a successful bounded invocation of an explicitly pinned worker can
    /// produce this token. Bytes, pin and provenance become durable together.
    pub fn put_sanitized_image(
        &mut self,
        stage: &crate::artifact::Stage,
        image: &dext_native::image::SanitizedImage,
    ) -> Result<JpegDescriptor> {
        self.put_sanitized_image_checked(stage, image, false, |_, _| Ok(()))
    }

    pub(crate) fn put_sanitized_image_checked<F>(
        &mut self,
        stage: &crate::artifact::Stage,
        image: &dext_native::image::SanitizedImage,
        allow_cancelled: bool,
        record: F,
    ) -> Result<JpegDescriptor>
    where
        F: FnOnce(&rusqlite::Transaction<'_>, &JpegDescriptor) -> Result<()>,
    {
        use rusqlite::{OptionalExtension, params};
        let worker = hex_digest(image.worker_digest());
        let source = hex_digest(image.source_digest());
        let digest = self.put_object_checked(stage, image.bytes(), allow_cancelled, |transaction, digest| {
            let existing: Option<(u64, u64)> = transaction.query_row(
                "SELECT width,height FROM image_provenance WHERE scope=?1 AND digest=?2 AND worker=?3 AND source=?4",
                params![stage.scope, digest, worker, source],
                |row| Ok((crate::nonnegative_id(row, 0)?, crate::nonnegative_id(row, 1)?)),
            ).optional()?;
            if let Some(dimensions) = existing {
                if dimensions != (u64::from(image.width()), u64::from(image.height())) {
                    return Err(Error::Incompatible);
                }
            } else {
                let count: i64 = transaction.query_row("SELECT count(*) FROM image_provenance", [], |row| row.get(0))?;
                if count >= crate::artifact::MAX_STAGES { return Err(Error::Exhausted); }
                crate::execute_one(transaction,
                    "INSERT INTO image_provenance VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![stage.scope, digest, worker, source, image.width(), image.height(), stage.owner_epoch as i64],
                )?;
            }
            record(transaction, &JpegDescriptor {
                object: ObjectReference { digest: digest.into(), byte_length: image.bytes().len() as u64 },
                width: image.width(),
                height: image.height(),
            })
        })?;
        Ok(JpegDescriptor {
            object: ObjectReference {
                digest,
                byte_length: image.bytes().len() as u64,
            },
            width: image.width(),
            height: image.height(),
        })
    }
}

impl crate::Store {
    /// Requires durable evidence that the object was produced by a bounded
    /// invocation of the currently approved worker. It does not grant disclosure.
    pub fn commit_verified_image_receipt_proposal(
        &mut self,
        proposal: &crate::transition::Proposal,
        attempt: &crate::completion::Attempt,
        intent_revision: u64,
        intent: &crate::transition::EffectIntent,
        result: &[u8],
        approved_worker: [u8; 32],
    ) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        let receipt = ImageReceipt::decode(result)?;
        let objects = receipt.objects();
        self.commit_checked_receipt_proposal(
            proposal,
            &crate::transition::ReceiptBinding {
                attempt,
                intent_revision,
                intent,
                result,
                objects: &objects,
            },
            |transaction| {
                validate_verified_images(transaction, &proposal.scope, &receipt, &approved_worker)?;
                Ok(())
            },
        )
    }

    #[cfg(feature = "dext-image-continuation")]
    /// Adopts only the deterministic DTC2 transition computed from the retained
    /// baseline and image receipt. Images do not create dependent dispatch intents.
    pub fn commit_image_tool_result_proposal(
        &mut self,
        proposal: &crate::transition::Proposal,
        attempt: &crate::completion::Attempt,
        intent_revision: u64,
        intent: &crate::transition::EffectIntent,
        result: &[u8],
        approved_worker: [u8; 32],
    ) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !proposal.effects.is_empty() || proposal.workspace.is_some() {
            return Err(Error::Invalid);
        }
        let receipt = ImageReceipt::decode(result)?;
        let objects = receipt.objects();
        self.commit_checked_receipt_proposal(
            proposal,
            &crate::transition::ReceiptBinding {
                attempt,
                intent_revision,
                intent,
                result,
                objects: &objects,
            },
            |transaction| {
                validate_verified_images(transaction, &proposal.scope, &receipt, &approved_worker)?;
                let (scope, engine, baseline): (String, String, Vec<u8>) = transaction.query_row(
                    "SELECT scope,engine,continuation FROM revisions WHERE revision=?1",
                    [proposal.expected_revision as i64],
                    |row| {
                        Ok((
                            crate::bounded_label(row, 0)?,
                            crate::bounded_label(row, 1)?,
                            crate::packed_blob(transaction, row, 2, crate::MAX_PAYLOAD)?,
                        ))
                    },
                )?;
                if scope != proposal.scope || engine != proposal.engine {
                    return Err(Error::Conflict);
                }
                let next = image_result_transition(&baseline, &scope, intent, &receipt)?;
                if next != proposal.continuation {
                    return Err(Error::Conflict);
                }
                Ok(())
            },
        )
    }

    /// Validates the exact retained receipt and immutable JPEG framing in the
    /// publication transaction. This neither authenticates the sanitizer nor
    /// authorizes disclosure. Generic trusted-host commit APIs remain available.
    pub fn commit_image_receipt_proposal(
        &mut self,
        proposal: &crate::transition::Proposal,
        attempt: &crate::completion::Attempt,
        intent_revision: u64,
        intent: &crate::transition::EffectIntent,
        result: &[u8],
    ) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        let receipt = ImageReceipt::decode(result)?;
        let objects = receipt.objects();
        self.commit_checked_receipt_proposal(
            proposal,
            &crate::transition::ReceiptBinding {
                attempt,
                intent_revision,
                intent,
                result,
                objects: &objects,
            },
            |transaction| validate_images(transaction, &proposal.scope, &receipt.images),
        )
    }
}

#[cfg(feature = "dext-image-continuation")]
/// Pure image-result proposal from a saved tool-wait baseline. The store checks
/// this exact transition again during adoption; this function grants no authority.
pub fn image_result_transition(
    baseline: &[u8],
    scope: &str,
    intent: &crate::transition::EffectIntent,
    receipt: &ImageReceipt,
) -> Result<Vec<u8>> {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let state = ToolResultsContinuation::restore(baseline).map_err(|_| Error::Invalid)?;
    let call = state
        .pending_calls()
        .map_err(|_| Error::Invalid)?
        .into_iter()
        .find(|call| call.id == intent.id && call.name == intent.adapter)
        .ok_or(Error::Conflict)?;
    if serde_json::to_vec(&call.input).map_err(|_| Error::Invalid)? != intent.input {
        return Err(Error::Conflict);
    }
    let blocks = receipt.result_blocks(scope, &call.id)?;
    state
        .observe_blocks(state.model_effect(), state.model_attempt(), &call, blocks)
        .map_err(|_| Error::Invalid)?
        .checkpoint()
        .map_err(|_| Error::Invalid)
}

pub(crate) fn validate_verified_images(
    db: &Connection,
    scope: &str,
    receipt: &ImageReceipt,
    approved_worker: &[u8; 32],
) -> Result<()> {
    validate_images(db, scope, &receipt.images)?;
    for image in &receipt.images {
        validate_provenance(db, scope, image, approved_worker)?;
    }
    Ok(())
}

fn validate_provenance(
    db: &Connection,
    scope: &str,
    image: &JpegDescriptor,
    approved_worker: &[u8; 32],
) -> Result<()> {
    use rusqlite::{OptionalExtension, params};
    let epoch: u64 = db.query_row("SELECT epoch FROM realm WHERE id=1", [], |row| {
        crate::nonnegative_id(row, 0)
    })?;
    let record: Option<(String, u64, u64, u64)> = db.query_row(
        "SELECT source,width,height,owner_epoch FROM image_provenance WHERE scope=?1 AND digest=?2 AND worker=?3 ORDER BY source LIMIT 1",
        params![scope, image.object.digest, hex_digest(approved_worker)],
        |row| Ok((crate::bounded_label(row, 0)?, crate::nonnegative_id(row, 1)?, crate::nonnegative_id(row, 2)?, crate::nonnegative_id(row, 3)?)),
    ).optional()?;
    let (source, width, height, owner) = record.ok_or(Error::Invalid)?;
    if !crate::artifact::valid_digest(&source) || owner == 0 || owner > epoch {
        return Err(Error::Incompatible);
    }
    if (width, height) != (u64::from(image.width), u64::from(image.height)) {
        return Err(Error::Conflict);
    }
    Ok(())
}

pub(crate) struct DisclosureState {
    context: Option<DisclosureContext>,
    identity: std::sync::Arc<()>,
}
impl Default for DisclosureState {
    fn default() -> Self {
        Self {
            context: None,
            identity: std::sync::Arc::new(()),
        }
    }
}

/// Current authenticated host policy input. Route must identify provider, endpoint,
/// contract and model; turn must include the logical lineage. This is not a grant.
#[derive(Clone, Eq, PartialEq)]
pub struct DisclosureContext {
    pub scope: String,
    pub route: String,
    pub turn: String,
    pub policy_generation: u64,
    pub approved_worker: [u8; 32],
}
impl std::fmt::Debug for DisclosureContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisclosureContext").finish_non_exhaustive()
    }
}

/// Ephemeral explicit approval. Cannot be constructed/deserialized from a saved
/// transcript. Revocation, context changes and reopening the store invalidate it.
pub struct DisclosureGrant {
    identity: std::sync::Arc<()>,
    context: DisclosureContext,
    image: JpegDescriptor,
}
impl std::fmt::Debug for DisclosureGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisclosureGrant").finish_non_exhaustive()
    }
}

impl crate::Store {
    /// Host revalidates route, privacy and turn identity before activating context.
    /// Every change invalidates all grants, even if the old context returns later.
    pub fn set_image_disclosure_context(&mut self, context: DisclosureContext) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(&context.scope)
            || !crate::valid_label(&context.turn)
            || !crate::artifact::valid_digest(&context.route)
        {
            self.revoke_image_disclosures();
            return Err(Error::Invalid);
        }
        if self.image_disclosure.context.as_ref() != Some(&context) {
            self.image_disclosure = DisclosureState {
                context: Some(context),
                identity: std::sync::Arc::new(()),
            };
        }
        Ok(())
    }

    pub fn revoke_image_disclosures(&mut self) {
        self.image_disclosure = DisclosureState::default();
    }

    /// Called only after a fresh host approval. Retention and previous receipts are
    /// not approval. The permission adapter, not a model, invokes this method.
    pub fn approve_image_disclosure(
        &mut self,
        image: &JpegDescriptor,
        context: &DisclosureContext,
    ) -> Result<DisclosureGrant> {
        if self.image_disclosure.context.as_ref() != Some(context) {
            return Err(Error::Stale);
        }
        self.disclosure_bytes(image, context)?;
        Ok(DisclosureGrant {
            identity: self.image_disclosure.identity.clone(),
            context: context.clone(),
            image: image.clone(),
        })
    }

    /// Native request adapters call immediately before serialization/dispatch.
    /// Caller context cannot roll authority back: it must match the current host
    /// context and the still-live grant identity. Returned bytes carry no authority
    /// to retain or send them in a different request context.
    pub fn image_bytes_for_disclosure(
        &mut self,
        grant: &DisclosureGrant,
        current: &DisclosureContext,
    ) -> Result<Vec<u8>> {
        if !std::sync::Arc::ptr_eq(&grant.identity, &self.image_disclosure.identity)
            || &grant.context != current
            || self.image_disclosure.context.as_ref() != Some(current)
        {
            return Err(Error::Stale);
        }
        self.disclosure_bytes(&grant.image, current)
    }

    fn disclosure_bytes(
        &mut self,
        image: &JpegDescriptor,
        context: &DisclosureContext,
    ) -> Result<Vec<u8>> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = (|| {
            let epoch: u64 =
                self.db
                    .query_row("SELECT epoch FROM realm WHERE id=1", [], |row| {
                        crate::nonnegative_id(row, 0)
                    })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            let cancelled: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
                [&context.scope],
                |row| row.get(0),
            )?;
            if cancelled {
                return Err(Error::Cancelled);
            }
            if !valid_dimensions(image) || !crate::artifact::valid_digest(&image.object.digest) {
                return Err(Error::Invalid);
            }
            validate_provenance(&self.db, &context.scope, image, &context.approved_worker)?;
            let bytes = crate::artifact::reference_bytes(&self.db, &context.scope, &image.object)?;
            if !sanitized_jpeg_matches_dimensions(&bytes, image.width, image.height) {
                return Err(Error::Incompatible);
            }
            Ok(bytes)
        })();
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }
}

fn validate_images(db: &Connection, scope: &str, images: &[JpegDescriptor]) -> Result<()> {
    for image in images {
        let bytes = crate::artifact::reference_bytes(db, scope, &image.object)?;
        if !sanitized_jpeg_matches_dimensions(&bytes, image.width, image.height) {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}
