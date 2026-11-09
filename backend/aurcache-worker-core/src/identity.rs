//! Worker cryptographic identity: a persisted keypair, a CSR sent at
//! enrollment, and the CA-signed leaf certificate returned on approval.
//!
//! The identity fingerprint is the SHA-256 of the DER-encoded
//! `SubjectPublicKeyInfo` — identical to what the server computes from the CSR
//! and later from the presented client certificate, so it is a stable handle
//! across the whole enrollment lifecycle.

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair, PublicKeyData};
use std::path::{Path, PathBuf};

const KEY_FILE: &str = "worker-key.pem";
const CERT_FILE: &str = "worker-cert.pem";
const CA_FILE: &str = "ca-cert.pem";

/// The worker's on-disk identity.
pub struct Identity {
    data_dir: PathBuf,
    key: KeyPair,
    /// SHA-256 hex of the SubjectPublicKeyInfo DER.
    pub fingerprint: String,
}

impl Identity {
    /// Load the persisted keypair, or generate and persist a fresh one.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let key_path = data_dir.join(KEY_FILE);

        let key = if key_path.exists() {
            let pem = std::fs::read_to_string(&key_path)
                .with_context(|| format!("reading {}", key_path.display()))?;
            KeyPair::from_pem(&pem).context("parsing persisted worker key")?
        } else {
            let key = KeyPair::generate().context("generating worker key")?;
            aurcache_common::fs::write_private(&key_path, key.serialize_pem().as_bytes())?;
            key
        };

        // `subject_public_key_info` is rcgen 0.14's name for what 0.13 called
        // `public_key_der`: the same SPKI DER, so the fingerprint -- which is
        // every worker's stable identity -- is unchanged by the upgrade.
        let fingerprint = aurcache_ca::fingerprint_from_spki_der(&key.subject_public_key_info());
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            key,
            fingerprint,
        })
    }

    /// Generate a PEM certificate signing request for this identity.
    pub fn generate_csr(&self, name: &str) -> Result<String> {
        let params =
            CertificateParams::new(vec![name.to_string()]).context("building CSR params")?;
        let csr = params
            .serialize_request(&self.key)
            .context("serializing CSR")?;
        csr.pem().context("encoding CSR pem")
    }

    /// Path to the signed leaf certificate (may not yet exist).
    pub fn cert_path(&self) -> PathBuf {
        self.data_dir.join(CERT_FILE)
    }

    /// Path to the pinned CA certificate (may not yet exist).
    pub fn ca_path(&self) -> PathBuf {
        self.data_dir.join(CA_FILE)
    }

    /// The private key encoded as PEM (for building the reqwest identity).
    pub fn key_pem(&self) -> String {
        self.key.serialize_pem()
    }

    /// True once we hold both a signed leaf cert and the CA cert on disk.
    pub fn is_enrolled(&self) -> bool {
        self.cert_path().exists() && self.ca_path().exists()
    }

    /// Persist the CA-signed leaf certificate and CA certificate.
    pub fn store_signed(&self, cert_pem: &str, ca_pem: &str) -> Result<()> {
        std::fs::write(self.cert_path(), cert_pem)
            .with_context(|| format!("writing {}", self.cert_path().display()))?;
        std::fs::write(self.ca_path(), ca_pem)
            .with_context(|| format!("writing {}", self.ca_path().display()))?;
        Ok(())
    }

    /// Read the stored leaf certificate PEM.
    pub fn cert_pem(&self) -> Result<String> {
        std::fs::read_to_string(self.cert_path())
            .with_context(|| format!("reading {}", self.cert_path().display()))
    }

    /// Read the stored CA certificate PEM.
    pub fn ca_pem(&self) -> Result<String> {
        std::fs::read_to_string(self.ca_path())
            .with_context(|| format!("reading {}", self.ca_path().display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.fingerprint.len(), 64);
    }

    /// The worker computes its fingerprint from its own keypair with `rcgen`;
    /// the server computes it from the CSR with `x509-parser`. They are two
    /// implementations of "SHA-256 of the SubjectPublicKeyInfo DER", and every
    /// worker's identity depends on them agreeing.
    ///
    /// Worth a test of its own because nothing else compares the two: the CA's
    /// own tests check `x509-parser` against itself, so a change on the rcgen
    /// side -- an upgrade renaming `public_key_der` to
    /// `subject_public_key_info`, say -- would pass everything and silently
    /// re-enroll every existing worker as a new machine.
    #[test]
    fn the_worker_and_the_server_agree_on_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(dir.path()).unwrap();
        let csr = identity.generate_csr("worker-1").unwrap();

        let from_server = aurcache_ca::fingerprint_from_csr_pem(&csr).unwrap();
        assert_eq!(
            identity.fingerprint, from_server,
            "the worker and the server derived different identities from one key"
        );
    }

    #[test]
    fn csr_contains_request() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();
        let csr = id.generate_csr("worker-1").unwrap();
        assert!(csr.contains("CERTIFICATE REQUEST"));
    }

    #[test]
    fn not_enrolled_until_certs_stored() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();
        assert!(!id.is_enrolled());
        id.store_signed("LEAF", "CA").unwrap();
        assert!(id.is_enrolled());
        assert_eq!(id.cert_pem().unwrap(), "LEAF");
        assert_eq!(id.ca_pem().unwrap(), "CA");
    }
}
