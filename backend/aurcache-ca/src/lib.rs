//! AURCache internal certificate authority.
//!
//! On first startup AURCache generates a self-signed CA (persisted in the data
//! volume). It issues its own server certificate from that CA and signs worker
//! certificate signing requests (CSRs) when a worker is approved. Workers are
//! identified by the SHA-256 fingerprint of their public key (SubjectPublicKeyInfo),
//! which is stable across the CSR, the issued certificate, and the certificate
//! presented during the mTLS handshake.

use anyhow::{Context, anyhow};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, PublicKeyData,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use time::{Duration, OffsetDateTime};

const CA_CERT_FILE: &str = "ca-cert.pem";
const CA_KEY_FILE: &str = "ca-key.pem";

/// Where the CA certificate is stored in `dir`.
#[must_use]
pub fn cert_path(dir: &Path) -> PathBuf {
    dir.join(CA_CERT_FILE)
}

/// Where the CA private key is stored in `dir`.
#[must_use]
pub fn key_path(dir: &Path) -> PathBuf {
    dir.join(CA_KEY_FILE)
}

/// A loaded (or freshly created) internal CA: its certificate and private key.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Ca {
    cert_pem: String,
    key_pem: String,
}

/// A server certificate and its private key, both PEM.
pub struct ServerCert {
    pub cert_pem: String,
    pub key_pem: String,
}

/// The result of signing a worker CSR.
pub struct SignedWorkerCert {
    /// PEM of the signed leaf certificate.
    pub cert_pem: String,
    /// Epoch seconds when the certificate expires.
    pub not_after: i64,
    /// SHA-256 fingerprint of the worker's public key.
    pub fingerprint: String,
}

impl Ca {
    /// Load a validated CA, creating one only when no identity exists.
    ///
    /// The PEM files are the CA, as an operator sees and installs it; a valid
    /// pair there is used as it is. `ca-identity.json` is a copy written after
    /// them, so a pair left half-written -- by an interrupted install or a
    /// rotation -- is repaired from it rather than rotating the identity.
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).context("creating CA directory")?;
        let ca = Self::read_identity(dir)?.map_or_else(Self::generate_ca, Ok)?;
        ca.persist(dir)?;
        Ok(ca)
    }

    /// Read a validated identity without creating, rotating or repairing files.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        Self::read_identity(dir)?.ok_or_else(|| anyhow!("no CA identity at {}", dir.display()))
    }

    fn read_identity(dir: &Path) -> anyhow::Result<Option<Self>> {
        let recorded = match std::fs::read(dir.join("ca-identity.json")) {
            Ok(bytes) => {
                Some(serde_json::from_slice::<Self>(&bytes).context("reading CA identity")?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("reading CA identity"),
        };
        let pems = (
            std::fs::read_to_string(cert_path(dir)),
            std::fs::read_to_string(key_path(dir)),
        );
        let installed = match pems {
            (Ok(cert_pem), Ok(key_pem)) => {
                let ca = Self { cert_pem, key_pem };
                match ca.validate() {
                    Ok(()) => Some(ca),
                    Err(error) if recorded.is_some() => {
                        tracing::warn!("CA files are not a valid pair ({error:#}); restoring them");
                        None
                    }
                    Err(error) => return Err(error),
                }
            }
            (Err(cert), Err(key))
                if cert.kind() == std::io::ErrorKind::NotFound
                    && key.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            _ if recorded.is_some() => None,
            _ => anyhow::bail!(
                "incomplete or unreadable CA identity; restore its certificate and key together"
            ),
        };
        let ca = match (installed, recorded) {
            (Some(installed), Some(recorded)) if installed.cert_pem != recorded.cert_pem => {
                tracing::info!("Using the CA installed in {}", dir.display());
                installed
            }
            (Some(installed), _) => installed,
            (None, Some(recorded)) => recorded,
            (None, None) => return Ok(None),
        };
        ca.validate()?;
        Ok(Some(ca))
    }

    /// Install a restored certificate and key after validating their pairing.
    pub fn restore(dir: &Path, cert_pem: String, key_pem: String) -> anyhow::Result<Self> {
        let ca = Self { cert_pem, key_pem };
        ca.validate()?;
        std::fs::create_dir_all(dir)?;
        ca.persist(dir)?;
        Ok(ca)
    }

    /// Explicitly replace the CA, invalidating certificates issued by the old CA.
    pub fn rotate(dir: &Path) -> anyhow::Result<Self> {
        let ca = Self::generate_ca()?;
        std::fs::create_dir_all(dir)?;
        ca.persist(dir)?;
        Ok(ca)
    }

    fn validate(&self) -> anyhow::Result<()> {
        use x509_parser::prelude::FromDer;
        let key = KeyPair::from_pem(&self.key_pem).context("parsing persisted CA key")?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(self.cert_pem.as_bytes())
            .map_err(|error| anyhow!("parsing persisted CA certificate: {error}"))?;
        if pem.label != "CERTIFICATE" {
            anyhow::bail!("expected a CA certificate");
        }
        let (_, cert) = x509_parser::certificate::X509Certificate::from_der(&pem.contents)
            .map_err(|error| anyhow!("parsing persisted CA certificate: {error}"))?;
        if cert.public_key().raw != key.subject_public_key_info() {
            anyhow::bail!("CA certificate and private key do not match");
        }
        if !cert.is_ca() {
            anyhow::bail!("persisted certificate is not a CA");
        }
        Ok(())
    }

    fn persist(&self, dir: &Path) -> anyhow::Result<()> {
        use aurcache_common::fs::write_private;
        // The pair first, the copy last. Interrupted between the two PEMs, the
        // pair does not match and the copy -- still the previous identity --
        // repairs it. Interrupted after both, the new pair is valid and wins.
        write_private(&key_path(dir), self.key_pem.as_bytes())?;
        write_private(&cert_path(dir), self.cert_pem.as_bytes())?;
        write_private(&dir.join("ca-identity.json"), &serde_json::to_vec(self)?)?;
        Ok(())
    }

    fn generate_ca() -> anyhow::Result<Self> {
        let key = KeyPair::generate().context("generating CA key")?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "AURCache Internal CA");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
        params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
        let cert = params.self_signed(&key).context("self-signing CA")?;
        Ok(Self {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        })
    }

    /// PEM of the CA certificate (served to workers so they can pin it).
    #[must_use]
    pub fn ca_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// PEM of the private key, for an explicitly requested credential backup.
    #[must_use]
    pub fn private_key_pem(&self) -> &str {
        &self.key_pem
    }

    /// Whether this CA issued `cert_pem`.
    ///
    /// A real signature check against the CA's public key, not a comparison of
    /// issuer names: the point is to catch a certificate signed by a *previous* CA,
    /// and a regenerated CA keeps the same subject.
    ///
    /// Any certificate that cannot be parsed answers `false`. This only ever
    /// decides whether to re-issue, so treating the unreadable as not-ours costs a
    /// signature and is the safe direction.
    #[must_use]
    pub fn issued(&self, cert_pem: &str) -> bool {
        use x509_parser::prelude::FromDer;

        let Ok(ca_der) = pem_to_der(&self.cert_pem) else {
            return false;
        };
        let Ok((_, ca)) = x509_parser::certificate::X509Certificate::from_der(&ca_der) else {
            return false;
        };
        let Ok(leaf_der) = pem_to_der(cert_pem) else {
            return false;
        };
        let Ok((_, leaf)) = x509_parser::certificate::X509Certificate::from_der(&leaf_der) else {
            return false;
        };
        leaf.verify_signature(Some(ca.public_key())).is_ok()
    }

    /// SHA-256 fingerprint of the CA certificate (hex, lowercase).
    pub fn ca_cert_fingerprint(&self) -> anyhow::Result<String> {
        cert_fingerprint(&self.cert_pem)
    }

    /// Reconstruct an issuer usable for signing from the persisted CA material.
    ///
    /// rcgen 0.14 models this directly: the CA certificate and the key that
    /// signs with it are one `Issuer`, where before they were a pair that every
    /// call site had to keep together and pass in the right order.
    fn issuer(&self) -> anyhow::Result<Issuer<'static, KeyPair>> {
        let ca_key = KeyPair::from_pem(&self.key_pem).context("parsing CA key")?;
        Issuer::from_ca_cert_pem(&self.cert_pem, ca_key).context("parsing CA cert for signing")
    }

    /// Issue a server certificate for the given subject alt names, signed by the CA.
    pub fn issue_server_cert(&self, sans: Vec<String>) -> anyhow::Result<ServerCert> {
        let issuer = self.issuer()?;
        let key = KeyPair::generate().context("generating server key")?;
        let mut params = CertificateParams::new(sans)?;
        params
            .distinguished_name
            .push(DnType::CommonName, "aurcache");
        params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
        params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
        let cert = params
            .signed_by(&key, &issuer)
            .context("signing server cert")?;
        Ok(ServerCert {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        })
    }

    /// Sign a worker CSR, returning the issued certificate and its metadata.
    pub fn sign_worker_csr(
        &self,
        csr_pem: &str,
        validity_days: i64,
    ) -> anyhow::Result<SignedWorkerCert> {
        let fingerprint = fingerprint_from_csr_pem(csr_pem)?;
        let issuer = self.issuer()?;

        let mut csr =
            CertificateSigningRequestParams::from_pem(csr_pem).context("parsing worker CSR")?;
        let not_after = OffsetDateTime::now_utc() + Duration::days(validity_days);
        csr.params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
        csr.params.not_after = not_after;

        let cert = csr.signed_by(&issuer).context("signing worker CSR")?;
        Ok(SignedWorkerCert {
            cert_pem: cert.pem(),
            not_after: not_after.unix_timestamp(),
            fingerprint,
        })
    }
}

/// A fingerprint as an operator may write it -- `AB:CD:…`, spaced, either
/// case -- in the form this crate computes them, so the two compare equal.
#[must_use]
pub fn normalize_fingerprint(fingerprint: &str) -> String {
    fingerprint.to_lowercase().replace([':', ' '], "")
}

/// SHA-256 fingerprint (hex, lowercase) of a whole certificate PEM: what a
/// worker pins the server's CA by.
///
/// Lowercase hex without separators; see [`normalize_fingerprint`] for
/// comparing one an operator typed.
///
/// The PEM must be a `CERTIFICATE` block: a private key handed in by mistake
/// fails here rather than producing a fingerprint nothing will ever match.
pub fn cert_fingerprint(cert_pem: &str) -> anyhow::Result<String> {
    let (_, doc) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| anyhow!("parsing PEM: {e}"))?;
    if doc.label != "CERTIFICATE" {
        anyhow::bail!("expected a CERTIFICATE block, found {}", doc.label);
    }
    Ok(sha256_hex(&doc.contents))
}

/// SHA-256 fingerprint (hex, lowercase) of the public key in a CSR PEM.
pub fn fingerprint_from_csr_pem(csr_pem: &str) -> anyhow::Result<String> {
    use x509_parser::prelude::FromDer;
    let der = pem_to_der(csr_pem)?;
    let (_, csr) = x509_parser::certification_request::X509CertificationRequest::from_der(&der)
        .map_err(|e| anyhow!("parsing CSR: {e}"))?;
    Ok(sha256_hex(csr.certification_request_info.subject_pki.raw))
}

fn pem_to_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    let (_, doc) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| anyhow!("parsing PEM: {e}"))?;
    Ok(doc.contents)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// SHA-256 fingerprint (hex, lowercase) of a raw SubjectPublicKeyInfo DER slice.
///
/// Used by the mTLS auth guard, which obtains the SPKI bytes directly from the
/// verified client certificate. Matches the fingerprint computed from the CSR
/// and issued certificate.
#[must_use]
pub fn fingerprint_from_spki_der(spki_der: &[u8]) -> String {
    sha256_hex(spki_der)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 fingerprint (hex, lowercase) of the public key in a certificate
    /// PEM, to check an issued certificate against the CSR it was signed from.
    fn fingerprint_from_cert_pem(cert_pem: &str) -> anyhow::Result<String> {
        let der = pem_to_der(cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&der)
            .map_err(|e| anyhow!("parsing certificate: {e}"))?;
        Ok(sha256_hex(cert.tbs_certificate.subject_pki.raw))
    }

    /// A certificate this CA signed is recognised as its own.
    #[test]
    fn a_ca_recognises_the_certificates_it_issued() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();

        let key = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["worker-1".to_string()]).unwrap();
        let csr_pem = params.serialize_request(&key).unwrap().pem().unwrap();
        let signed = ca.sign_worker_csr(&csr_pem, 365).unwrap();

        assert!(ca.issued(&signed.cert_pem));
    }

    /// And one signed by a *different* CA is not -- which is the whole point.
    ///
    /// A regenerated CA keeps the same subject name, so this has to be a real
    /// signature check: comparing issuer names would call the old certificate
    /// ours and leave the worker presenting something nothing can verify.
    #[test]
    fn a_ca_rejects_a_certificate_from_a_previous_ca() {
        let old_dir = tempfile::tempdir().unwrap();
        let old_ca = Ca::load_or_create(old_dir.path()).unwrap();

        let key = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["worker-1".to_string()]).unwrap();
        let csr_pem = params.serialize_request(&key).unwrap().pem().unwrap();
        let from_old = old_ca.sign_worker_csr(&csr_pem, 365).unwrap();

        // A fresh CA, as a deployment that lost its data directory would build.
        let new_dir = tempfile::tempdir().unwrap();
        let new_ca = Ca::load_or_create(new_dir.path()).unwrap();

        assert!(
            !new_ca.issued(&from_old.cert_pem),
            "a certificate from the previous CA must be re-issued, not reused"
        );
        assert!(new_ca.issued(&new_ca.sign_worker_csr(&csr_pem, 365).unwrap().cert_pem));
    }

    /// Garbage is not ours, and must not panic on the way to saying so.
    #[test]
    fn unparseable_input_is_not_ours() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();
        assert!(!ca.issued(""));
        assert!(!ca.issued("-----BEGIN CERTIFICATE-----\nnonsense\n-----END CERTIFICATE-----\n"));
    }

    /// A key is not a certificate, whatever bytes it hashes to.
    #[test]
    fn only_a_certificate_has_a_certificate_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();
        assert!(cert_fingerprint(ca.ca_cert_pem()).is_ok());
        assert!(cert_fingerprint(&ca.key_pem).is_err());
    }

    #[test]
    fn creates_and_reloads_stable_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca1 = Ca::load_or_create(dir.path()).unwrap();
        let ca2 = Ca::load_or_create(dir.path()).unwrap();
        assert_eq!(ca1.ca_cert_pem(), ca2.ca_cert_pem());
        assert!(ca1.ca_cert_pem().contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn issues_server_cert_chaining_to_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();
        let ServerCert { cert_pem, key_pem } =
            ca.issue_server_cert(vec!["localhost".to_string()]).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn signs_worker_csr_with_stable_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();

        // Worker side: generate a keypair + CSR.
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["worker-1".to_string()]).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "worker-1");
        let csr_pem = params.serialize_request(&key).unwrap().pem().unwrap();

        let csr_fp = fingerprint_from_csr_pem(&csr_pem).unwrap();

        let signed = ca.sign_worker_csr(&csr_pem, 365).unwrap();
        assert!(signed.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(signed.not_after > 0);

        // The fingerprint is stable across CSR -> issued cert.
        let cert_fp = fingerprint_from_cert_pem(&signed.cert_pem).unwrap();
        assert_eq!(csr_fp, cert_fp);
        assert_eq!(signed.fingerprint, cert_fp);
    }

    #[test]
    fn different_workers_get_different_fingerprints() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();

        let make_csr = || {
            let key = KeyPair::generate().unwrap();
            let params = CertificateParams::new(vec!["w".to_string()]).unwrap();
            params.serialize_request(&key).unwrap().pem().unwrap()
        };

        let a = ca.sign_worker_csr(&make_csr(), 365).unwrap();
        let b = ca.sign_worker_csr(&make_csr(), 365).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_ne!(a.cert_pem, b.cert_pem);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[test]
    fn legacy_partial_invalid_and_mismatched_identities_are_rejected() {
        let ca = Ca::generate_ca().unwrap();
        let other = Ca::generate_ca().unwrap();
        for (cert, key) in [
            (Some(ca.cert_pem.as_str()), None),
            (None, Some(ca.key_pem.as_str())),
            (Some("garbage"), Some(ca.key_pem.as_str())),
            (Some(ca.cert_pem.as_str()), Some(other.key_pem.as_str())),
        ] {
            let dir = tempfile::tempdir().unwrap();
            if let Some(cert) = cert {
                std::fs::write(cert_path(dir.path()), cert).unwrap();
            }
            if let Some(key) = key {
                std::fs::write(key_path(dir.path()), key).unwrap();
            }
            assert!(Ca::load_or_create(dir.path()).is_err());
            assert!(!dir.path().join("ca-identity.json").exists());
        }
    }

    #[test]
    fn a_ca_installed_by_replacing_its_files_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        Ca::load_or_create(dir.path()).unwrap();
        let installed = Ca::generate_ca().unwrap();
        std::fs::write(cert_path(dir.path()), &installed.cert_pem).unwrap();
        std::fs::write(key_path(dir.path()), &installed.key_pem).unwrap();

        let reloaded = Ca::load_or_create(dir.path()).unwrap();
        assert_eq!(reloaded.cert_pem, installed.cert_pem);
        assert_eq!(
            std::fs::read_to_string(cert_path(dir.path())).unwrap(),
            installed.cert_pem
        );
        assert_eq!(Ca::load(dir.path()).unwrap().cert_pem, installed.cert_pem);
    }

    #[test]
    fn interrupted_pem_installation_recovers_the_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();
        std::fs::remove_file(key_path(dir.path())).unwrap();
        std::fs::write(cert_path(dir.path()), "interrupted").unwrap();
        let reloaded = Ca::load_or_create(dir.path()).unwrap();
        assert_eq!(ca.cert_pem, reloaded.cert_pem);
        assert_eq!(
            std::fs::read_to_string(key_path(dir.path())).unwrap(),
            ca.key_pem
        );
        assert_eq!(
            std::fs::read_to_string(cert_path(dir.path())).unwrap(),
            ca.cert_pem
        );
    }
}
