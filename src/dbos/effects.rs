//! Minimal actual-effect intent/receipt guard. No execution position or retry lifecycle.
use super::admission::{AdmissionStore, Envelope, Error, Invocation, Result, ScopedReference};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

pub enum EffectAccess<'a> {
    Verified(ScopedReference),
    Execute(EffectPermit<'a>),
}
pub struct EffectPermit<'a> {
    store: &'a AdmissionStore,
    envelope: Box<Envelope>,
    action: String,
    key: String,
}
/// A consumed outbound reservation. Recording its observation does not grant
/// permission to publish or perform another effect.
pub struct OutboundEffect<'a>(EffectPermit<'a>);
fn refused(reason: &'static str) -> Error {
    Error::Invalid(reason)
}
fn current(
    invocation: &Invocation,
    action: &str,
    authorize: impl FnOnce(&Invocation, &str) -> Result<()>,
) -> Result<()> {
    invocation.validate()?;
    if invocation.action_sha256() != action {
        return Err(refused(
            "actual effect differs from admitted exact action identity",
        ));
    }
    if invocation
        .authorization_expires_at_ms()
        .is_some_and(|at| chrono::Utc::now().timestamp_millis() >= at)
    {
        return Err(refused("effect authorization expired"));
    }
    authorize(invocation, action)
}
impl AdmissionStore {
    pub fn effect<'a>(
        &'a self,
        envelope: &Envelope,
        actual_action: &str,
        authorize: impl FnOnce(&Invocation, &str) -> Result<()>,
        verify_receipt: impl FnOnce(&ScopedReference) -> Result<()>,
    ) -> Result<EffectAccess<'a>> {
        self.verify(envelope)?;
        current(&envelope.invocation, actual_action, authorize)?;
        let bytes = crate::canonical::to_vec(&serde_json::json!([
            envelope.dbos_id.as_str(),
            envelope.invocation_sha256.as_str(),
            actual_action,
        ]))
        .expect("string tuple");
        let key = format!("forge-effect-v1-{:x}", Sha256::digest(bytes));
        let existing = self.write(|connection| {
            let existing: Option<(String, String, String, String, Option<String>)> = connection
                .query_row(
                    "SELECT dbos_id,invocation_digest,action_digest,state,receipt FROM dbos_effect_receipts WHERE effect_key=?1",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            if let Some((id, digest, action, state, receipt)) = existing {
                if id != envelope.dbos_id
                    || digest != envelope.invocation_sha256
                    || action != actual_action
                {
                    return Err(refused("effect receipt identity conflict"));
                }
                if state != "verified" {
                    return Err(refused("external effect outcome is uncertain; reconciliation required before any retry"));
                }
                let receipt: ScopedReference = serde_json::from_str(
                    receipt.as_deref().ok_or_else(|| refused("verified receipt missing"))?,
                )
                .map_err(|_| refused("verified receipt invalid"))?;
                receipt.validate(envelope.invocation.scope())?;
                return Ok(Some(receipt));
            }
            connection.execute("INSERT INTO dbos_effect_receipts(effect_key,dbos_id,invocation_digest,action_digest,state) VALUES(?1,?2,?3,?4,'uncertain')",params![key,envelope.dbos_id,envelope.invocation_sha256,actual_action])?;
            Ok(None)
        })?;
        if let Some(receipt) = existing {
            verify_receipt(&receipt)?;
            return Ok(EffectAccess::Verified(receipt));
        }
        Ok(EffectAccess::Execute(EffectPermit {
            store: self,
            envelope: Box::new(envelope.clone()),
            action: actual_action.to_owned(),
            key,
        }))
    }
}
impl<'a> EffectPermit<'a> {
    pub fn idempotency_key(&self) -> &str {
        &self.key
    }
    pub fn before_outbound(
        self,
        authorize: impl FnOnce(&Invocation, &str) -> Result<()>,
    ) -> Result<OutboundEffect<'a>> {
        current(&self.envelope.invocation, &self.action, authorize)?;
        Ok(OutboundEffect(self))
    }
}
impl OutboundEffect<'_> {
    pub fn idempotency_key(&self) -> &str {
        &self.0.key
    }
    /// Retain a verified observation even if authority expired during the call.
    /// A later reuse/publication must independently check current authority.
    pub fn complete(
        self,
        receipt: ScopedReference,
        verify_receipt: impl FnOnce(&ScopedReference) -> Result<()>,
    ) -> Result<ScopedReference> {
        let permit = self.0;
        permit.store.verify(&permit.envelope)?;
        receipt.validate(permit.envelope.invocation.scope())?;
        verify_receipt(&receipt)?;
        permit.store.write(|connection| {
            let updated = connection.execute(
                "UPDATE dbos_effect_receipts SET state='verified',receipt=?2 WHERE effect_key=?1 AND dbos_id=?3 AND invocation_digest=?4 AND action_digest=?5 AND state='uncertain'",
                params![permit.key, serde_json::to_string(&receipt).expect("closed receipt"),
                    permit.envelope.dbos_id, permit.envelope.invocation_sha256, permit.action],
            )?;
            if updated != 1 {
                return Err(refused("effect receipt publication lost its exact intent"));
            }
            Ok(())
        })?;
        Ok(receipt)
    }
}

impl AdmissionStore {
    fn effect_key(envelope: &Envelope) -> String {
        let bytes = crate::canonical::to_vec(&serde_json::json!([
            envelope.dbos_id.as_str(),
            envelope.invocation_sha256.as_str(),
            envelope.invocation.action_sha256(),
        ]))
        .expect("string tuple");
        format!("forge-effect-v1-{:x}", Sha256::digest(bytes))
    }
    pub fn effect_observation(&self, envelope: &Envelope) -> Result<Option<serde_json::Value>> {
        self.verify(envelope)?;
        self.write(|c|{let row=c.query_row("SELECT state,receipt FROM dbos_effect_receipts WHERE effect_key=?1",[Self::effect_key(envelope)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?))).optional()?;
            row.map(|(state,raw)|Ok(serde_json::json!({"state":if state=="verified"{"completed"}else{&state},"receipt":raw.map(|s|serde_json::from_str::<ScopedReference>(&s)).transpose().map_err(|_|refused("invalid effect receipt"))?}))).transpose()})
    }
    pub fn effect_decision(&self, envelope: &Envelope) -> Result<Option<serde_json::Value>> {
        self.verify(envelope)?;
        self.write(|c|Ok(c.query_row("SELECT actor,decision,note,created_ms FROM dbos_effect_decisions WHERE effect_key=?1",[Self::effect_key(envelope)],|r|Ok(serde_json::json!({"actor":r.get::<_,String>(0)?,"decision":r.get::<_,String>(1)?,"note":r.get::<_,String>(2)?,"created_ms":r.get::<_,i64>(3)?}))).optional()?))
    }
    /// Caller holds the same live effect lock used by outbound. This terminal
    /// sentinel is permanent for this invocation; a late original cannot send.
    pub fn fence_unstarted(&self, envelope: &Envelope) -> Result<()> {
        self.verify(envelope)?;
        self.write(|c|{c.execute("INSERT OR IGNORE INTO dbos_effect_receipts(effect_key,dbos_id,invocation_digest,action_digest,state) VALUES(?1,?2,?3,?4,'not_applied')",params![Self::effect_key(envelope),envelope.dbos_id,envelope.invocation_sha256,envelope.invocation.action_sha256()])?;Ok(())})
    }
    /// Application reconciliation must first establish terminality, settlement,
    /// current operator authority and exclusive ownership of the effect fence.
    pub fn reconcile_effect(
        &self,
        envelope: &Envelope,
        receipt: Option<&ScopedReference>,
        actor: &str,
        note: &str,
    ) -> Result<()> {
        self.verify(envelope)?;
        if actor != envelope.invocation.principal() || note.trim().is_empty() || note.len() > 4096 {
            return Err(refused(
                "scoped actor and bounded provider evidence required",
            ));
        }
        if let Some(r) = receipt {
            r.validate(envelope.invocation.scope())?;
        }
        self.write(|c|{let n=c.execute("UPDATE dbos_effect_receipts SET state=?2,receipt=?3 WHERE effect_key=?1 AND state='uncertain'",params![Self::effect_key(envelope),if receipt.is_some(){"verified"}else{"not_applied"},receipt.map(serde_json::to_string).transpose().map_err(|_|refused("invalid receipt"))?])?;if n!=1{return Err(refused("no unresolved effect"));}c.execute("INSERT INTO dbos_effect_decisions(effect_key,actor,decision,note,created_ms) VALUES(?1,?2,?3,?4,?5)",params![Self::effect_key(envelope),actor,if receipt.is_some(){"completed"}else{"not_applied"},note,chrono::Utc::now().timestamp_millis()])?;Ok(())})
    }
}
