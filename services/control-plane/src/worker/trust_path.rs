//! Derive signing keys only from a validated client path, excluding unused certificates supplied by a peer.
use rustls_pki_types::{CertificateDer, TrustAnchor, UnixTime, pem::PemObject};
use sha2::{Digest, Sha256};
use tonic::Status;

pub(super) struct Roots {
    anchors: Vec<TrustAnchor<'static>>,
    public_keys: Vec<String>,
}
impl Roots {
    pub(super) fn from_pem(bytes: &[u8]) -> Result<Self, ()> {
        let mut roots = Self {
            anchors: Vec::new(),
            public_keys: Vec::new(),
        };
        for item in CertificateDer::pem_slice_iter(bytes) {
            let der = item.map_err(|_| ())?;
            let (tail, cert) = x509_parser::parse_x509_certificate(der.as_ref()).map_err(|_| ())?;
            if !tail.is_empty() {
                return Err(());
            }
            let anchor = webpki::anchor_from_trusted_cert(&der)
                .map_err(|_| ())?
                .to_owned();
            roots.anchors.push(anchor);
            roots
                .public_keys
                .push(hex::encode(Sha256::digest(cert.public_key().raw)));
        }
        if roots.anchors.is_empty() {
            return Err(());
        }
        Ok(roots)
    }
    pub(super) fn signing_keys(
        &self,
        certificates: &[CertificateDer<'static>],
    ) -> Result<Vec<String>, Status> {
        let invalid = || Status::unauthenticated("verified client certificate path required");
        let (leaf, intermediates) = certificates.split_first().ok_or_else(invalid)?;
        let cert = webpki::EndEntityCert::try_from(leaf).map_err(|_| invalid())?;
        let path = cert
            .verify_for_usage(
                webpki::ALL_VERIFICATION_ALGS,
                &self.anchors,
                intermediates,
                UnixTime::now(),
                webpki::KeyUsage::client_auth(),
                None,
                None,
            )
            .map_err(|_| invalid())?;
        let anchor = self
            .anchors
            .iter()
            .position(|a| std::ptr::eq(a, path.anchor()))
            .ok_or_else(invalid)?;
        let mut keys = path
            .intermediate_certificates()
            .map(|c| hex::encode(Sha256::digest(c.subject_public_key_info().as_ref())))
            .collect::<Vec<_>>();
        keys.push(self.public_keys[anchor].clone());
        keys.sort_unstable();
        keys.dedup();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
        ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PublicKeyData,
    };

    fn authority(name: &str) -> CertificateParams {
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
    }

    #[test]
    fn derives_only_the_signing_keys_on_the_verified_client_path() {
        let root_key = KeyPair::generate().expect("root key");
        let intermediate_key = KeyPair::generate().expect("intermediate key");
        let mut expected = vec![
            hex::encode(Sha256::digest(root_key.subject_public_key_info())),
            hex::encode(Sha256::digest(intermediate_key.subject_public_key_info())),
        ];
        expected.sort();
        let root = CertifiedIssuer::self_signed(authority("Root"), root_key).expect("root");
        let intermediate =
            CertifiedIssuer::signed_by(authority("Intermediate"), intermediate_key, &root)
                .expect("intermediate");
        let other = CertifiedIssuer::self_signed(
            authority("Other"),
            KeyPair::generate().expect("other key"),
        )
        .expect("other CA");
        let roots = Roots::from_pem(format!("{}{}", other.pem(), root.pem()).as_bytes())
            .expect("CA bundle");
        let mut params = CertificateParams::default();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let leaf_key = KeyPair::generate().expect("device key");
        let leaf = params
            .signed_by(&leaf_key, &intermediate)
            .expect("issued device");
        let chain = vec![
            leaf.der().clone(),
            other.der().clone(),
            intermediate.der().clone(),
        ];
        assert_eq!(
            roots.signing_keys(&chain).expect("validated path"),
            expected
        );
        assert!(roots.signing_keys(&chain[..2]).is_err());
        let foreign = Roots::from_pem(other.pem().as_bytes()).expect("unrelated trust root");
        assert!(foreign.signing_keys(&chain).is_err());
    }
}
