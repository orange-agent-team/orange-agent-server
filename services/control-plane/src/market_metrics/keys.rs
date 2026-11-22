//! Produce join keys inside an authorized extract. The secret is never serialized with the extract.
use super::ReportError;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

#[derive(Clone, Copy)]
pub enum Entity {
    Payer,
    Node,
    Slot,
    Job,
    Attempt,
    Order,
    Refund,
    Dispute,
}
impl Entity {
    fn label(self) -> &'static str {
        match self {
            Self::Payer => "payer",
            Self::Node => "node",
            Self::Slot => "slot",
            Self::Job => "job",
            Self::Attempt => "attempt",
            Self::Order => "order",
            Self::Refund => "refund",
            Self::Dispute => "dispute",
        }
    }
}
pub struct ExportKeys(Hmac<Sha256>);
impl ExportKeys {
    pub fn new(secret: &[u8; 32]) -> Result<Self, ReportError> {
        Hmac::<Sha256>::new_from_slice(secret)
            .map(Self)
            .map_err(|_| ReportError("invalid export key"))
    }
    fn digest(&self, purpose: &[u8], namespace: &str, source: &str) -> String {
        let mut mac = self.0.clone();
        mac.update(b"orange.market-report.v1\0");
        mac.update(purpose);
        mac.update(&[0]);
        mac.update(namespace.as_bytes());
        mac.update(&[0]);
        mac.update(source.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
    pub fn subject(&self, kind: Entity, source_id: &str) -> String {
        self.digest(b"subject", kind.label(), source_id)
    }
    pub fn receipt(&self, source_namespace: &str, source_id: &str) -> Result<String, ReportError> {
        if !orange_protocol::valid_id(source_namespace) || !orange_protocol::valid_id(source_id) {
            return Err(ReportError("invalid source receipt key"));
        }
        Ok(self.digest(b"receipt", source_namespace, source_id))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separates_roles_receipts_and_export_scopes() {
        let a = ExportKeys::new(&[1; 32]).expect("first scope");
        let b = ExportKeys::new(&[2; 32]).expect("second scope");
        let payer = a.subject(Entity::Payer, "user-1");
        assert_eq!(payer, a.subject(Entity::Payer, "user-1"));
        assert_ne!(payer, b.subject(Entity::Payer, "user-1"));
        assert_ne!(payer, a.subject(Entity::Order, "user-1"));
        assert_ne!(
            payer,
            a.receipt("payer", "user-1").expect("receipt namespace")
        );
        assert!(a.receipt("invalid\0scope", "user-1").is_err());
    }
}
