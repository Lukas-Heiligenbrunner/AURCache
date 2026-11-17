//! An SSH private key, read from the OpenSSH format `ssh-keygen` writes, and
//! held for signing.
//!
//! Signing is *ring*'s: constant-time, with RSA blinded, since a build may
//! ask for as many signatures as it likes over any data it chooses.

use super::wire::{Reader, Truncated, Writer};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use num_bigint::BigUint;
use ring::rand::SystemRandom;
use ring::rsa::{KeyPairComponents, PublicKeyComponents};
use ring::signature::{self, EcdsaKeyPair, Ed25519KeyPair, KeyPair, RsaKeyPair};
use zeroize::Zeroizing;

const BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
const END: &str = "-----END OPENSSH PRIVATE KEY-----";
const MAGIC: &[u8] = b"openssh-key-v1\0";

const ED25519: &str = "ssh-ed25519";
const RSA: &str = "ssh-rsa";
const RSA_SHA2_256: &str = "rsa-sha2-256";
const RSA_SHA2_512: &str = "rsa-sha2-512";
const NISTP256: &str = "ecdsa-sha2-nistp256";
const NISTP384: &str = "ecdsa-sha2-nistp384";

/// The hash an RSA signature is made with; SHA-1 is not one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RsaHash {
    Sha256,
    Sha512,
}

/// A key the worker signs with.
pub struct SshKey {
    /// The public key, as the agent protocol and `authorized_keys` name it.
    public: Vec<u8>,
    comment: String,
    signer: Signer,
    rng: SystemRandom,
}

enum Signer {
    Ed25519(Ed25519KeyPair),
    Rsa(RsaKeyPair),
    Ecdsa {
        name: &'static str,
        pair: EcdsaKeyPair,
    },
}

/// What a sign request cannot be answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsigned {
    /// An RSA signature with SHA-1, the protocol's default when no flag asks
    /// for SHA-2.
    RsaSha1,
    Failed,
}

impl SshKey {
    /// Read a private key in the OpenSSH format, unencrypted.
    pub fn parse(text: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(text).map_err(|_| anyhow!("the key is not text"))?;
        let Some(body) = text
            .split_once(BEGIN)
            .and_then(|(_, rest)| rest.split_once(END))
            .map(|(body, _)| body)
        else {
            if text.contains("-----BEGIN") {
                bail!(
                    "the key is not in the OpenSSH format; `ssh-keygen -p -N '' -f <file>` \
                     rewrites it in that format"
                );
            }
            bail!("the file holds no private key");
        };
        let armour: Zeroizing<String> =
            Zeroizing::new(body.chars().filter(|c| !c.is_ascii_whitespace()).collect());
        let decoded = Zeroizing::new(
            STANDARD
                .decode(armour.as_bytes())
                .map_err(|_| anyhow!("the key's base64 is broken"))?,
        );
        Self::decode(&decoded).map_err(|e| e.context("reading the key"))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let truncated = |Truncated| anyhow!("the key is truncated");
        let rest = bytes
            .strip_prefix(MAGIC)
            .ok_or_else(|| anyhow!("the key is not an OpenSSH private key"))?;
        let mut reader = Reader::new(rest);
        let cipher = reader.string().map_err(truncated)?;
        let _kdf = reader.string().map_err(truncated)?;
        let _kdf_options = reader.string().map_err(truncated)?;
        if cipher != b"none" {
            bail!("the key is protected by a passphrase, which the worker has no way to enter");
        }
        if reader.u32().map_err(truncated)? != 1 {
            bail!("the file holds more than one key");
        }
        let public = reader.string().map_err(truncated)?.to_vec();
        let private = reader.string().map_err(truncated)?;
        let mut private = Reader::new(private);
        let check = private.u32().map_err(truncated)?;
        if private.u32().map_err(truncated)? != check {
            bail!("the key is corrupt");
        }
        let kind = private.string().map_err(truncated)?;
        let kind = std::str::from_utf8(kind).map_err(|_| anyhow!("the key's type is not text"))?;
        let (signer, derived) = match kind {
            ED25519 => ed25519(&mut private)?,
            RSA => rsa(&mut private)?,
            NISTP256 => ecdsa(&mut private, NISTP256)?,
            NISTP384 => ecdsa(&mut private, NISTP384)?,
            other if other.starts_with("sk-") => bail!(
                "{other} is a security key, which needs a touch for every signature; a build \
                 cannot give one"
            ),
            other => bail!("{other} keys are not supported; use an ed25519 key"),
        };
        if derived != public {
            bail!("the key's public half does not match its private half");
        }
        let comment = private.string().map_err(truncated)?;
        Ok(Self {
            public,
            comment: String::from_utf8_lossy(comment).into_owned(),
            signer,
            rng: SystemRandom::new(),
        })
    }

    /// The public key, as the agent protocol names it.
    #[must_use]
    pub fn public(&self) -> &[u8] {
        &self.public
    }

    #[must_use]
    pub fn comment(&self) -> &str {
        &self.comment
    }

    /// The key's type, as `authorized_keys` names it.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match &self.signer {
            Signer::Ed25519(_) => ED25519,
            Signer::Rsa(_) => RSA,
            Signer::Ecdsa { name, .. } => name,
        }
    }

    /// The fingerprint `ssh-keygen -l` prints: `SHA256:` and the digest of
    /// the public key, in base64.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, &self.public);
        format!("SHA256:{}", STANDARD_NO_PAD.encode(digest.as_ref()))
    }

    /// The public key as a line of `authorized_keys`.
    #[must_use]
    pub fn authorized_key(&self) -> String {
        format!("{} {}", self.kind(), STANDARD.encode(&self.public))
    }

    /// Sign `data`, as an SSH signature blob. `rsa` is the hash an RSA key
    /// signs with; `None` asks for SHA-1, which is refused.
    pub fn sign(&self, data: &[u8], rsa: Option<RsaHash>) -> Result<Vec<u8>, Unsigned> {
        match &self.signer {
            Signer::Ed25519(pair) => Ok(Writer::new()
                .string(ED25519.as_bytes())
                .string(pair.sign(data).as_ref())
                .finish()),
            Signer::Rsa(pair) => {
                let (name, padding): (&str, &'static dyn signature::RsaEncoding) = match rsa {
                    Some(RsaHash::Sha256) => (RSA_SHA2_256, &signature::RSA_PKCS1_SHA256),
                    Some(RsaHash::Sha512) => (RSA_SHA2_512, &signature::RSA_PKCS1_SHA512),
                    None => return Err(Unsigned::RsaSha1),
                };
                let mut signed = vec![0; pair.public().modulus_len()];
                pair.sign(padding, &self.rng, data, &mut signed)
                    .map_err(|_| Unsigned::Failed)?;
                Ok(Writer::new()
                    .string(name.as_bytes())
                    .string(&signed)
                    .finish())
            }
            Signer::Ecdsa { name, pair } => {
                let signed = pair.sign(&self.rng, data).map_err(|_| Unsigned::Failed)?;
                let (r, s) = signed.as_ref().split_at(signed.as_ref().len() / 2);
                let inner = Writer::new().mpint(r).mpint(s).finish();
                Ok(Writer::new()
                    .string(name.as_bytes())
                    .string(&inner)
                    .finish())
            }
        }
    }
}

/// An Ed25519 key's private fields, and the public key they make.
fn ed25519(private: &mut Reader<'_>) -> Result<(Signer, Vec<u8>)> {
    let truncated = |Truncated| anyhow!("the key is truncated");
    let public = private.string().map_err(truncated)?;
    let secret = private.string().map_err(truncated)?;
    if public.len() != 32 || secret.len() != 64 {
        bail!("the ed25519 key has fields of the wrong length");
    }
    let pair = Ed25519KeyPair::from_seed_and_public_key(&secret[..32], public)
        .map_err(|e| anyhow!("the ed25519 key is not valid: {e}"))?;
    let derived = Writer::new()
        .string(ED25519.as_bytes())
        .string(pair.public_key().as_ref())
        .finish();
    Ok((Signer::Ed25519(pair), derived))
}

/// An RSA key's private fields, and the public key they make.
///
/// OpenSSH stores `n`, `e`, `d`, `iqmp`, `p` and `q`; *ring* takes the CRT
/// form with `p > q`, so the exponents and the coefficient are worked out
/// here, once, when the key is read.
///
/// The arithmetic is num-bigint's, whose values cannot be zeroized: copies of
/// the private factors are left in freed memory of the worker's process. That
/// process is the worker, which holds the key anyway; nothing of a build can
/// read its memory. The byte forms handed to *ring* are zeroized.
fn rsa(private: &mut Reader<'_>) -> Result<(Signer, Vec<u8>)> {
    let truncated = |Truncated| anyhow!("the key is truncated");
    let n = private.mpint().map_err(truncated)?;
    let e = private.mpint().map_err(truncated)?;
    let d = BigUint::from_bytes_be(private.mpint().map_err(truncated)?);
    let _iqmp = private.mpint().map_err(truncated)?;
    let first = BigUint::from_bytes_be(private.mpint().map_err(truncated)?);
    let second = BigUint::from_bytes_be(private.mpint().map_err(truncated)?);
    let (p, q) = if first > second {
        (first, second)
    } else {
        (second, first)
    };
    let one = BigUint::from(1u8);
    if q <= one {
        bail!("the RSA key is not valid");
    }
    let dp = &d % (&p - &one);
    let dq = &d % (&q - &one);
    let qinv = q
        .modinv(&p)
        .ok_or_else(|| anyhow!("the RSA key is not valid"))?;
    let secret = |value: &BigUint| Zeroizing::new(value.to_bytes_be());
    let (d, p, q, dp, dq, qinv) = (
        secret(&d),
        secret(&p),
        secret(&q),
        secret(&dp),
        secret(&dq),
        secret(&qinv),
    );
    let pair = RsaKeyPair::from_components(&KeyPairComponents {
        public_key: PublicKeyComponents { n, e },
        d: d.as_slice(),
        p: p.as_slice(),
        q: q.as_slice(),
        dP: dp.as_slice(),
        dQ: dq.as_slice(),
        qInv: qinv.as_slice(),
    })
    .map_err(|e| {
        anyhow!(
            "the RSA key cannot be used: {e} (a key of 2048 to 4096 bits, with a public \
             exponent of at least 65537, is needed)"
        )
    })?;
    let derived = Writer::new()
        .string(RSA.as_bytes())
        .mpint(e)
        .mpint(n)
        .finish();
    Ok((Signer::Rsa(pair), derived))
}

/// An ECDSA key's private fields, and the public key they make.
fn ecdsa(private: &mut Reader<'_>, name: &'static str) -> Result<(Signer, Vec<u8>)> {
    let truncated = |Truncated| anyhow!("the key is truncated");
    let (curve, algorithm, len) = match name {
        NISTP256 => ("nistp256", &signature::ECDSA_P256_SHA256_FIXED_SIGNING, 32),
        _ => ("nistp384", &signature::ECDSA_P384_SHA384_FIXED_SIGNING, 48),
    };
    if private.string().map_err(truncated)? != curve.as_bytes() {
        bail!("the {name} key names another curve");
    }
    let point = private.string().map_err(truncated)?;
    let scalar = private.mpint().map_err(truncated)?;
    if scalar.len() > len {
        bail!("the {name} key's private scalar is too long");
    }
    let mut fixed = Zeroizing::new(vec![0; len]);
    fixed[len - scalar.len()..].copy_from_slice(scalar);
    let pair = EcdsaKeyPair::from_private_key_and_public_key(
        algorithm,
        &fixed,
        point,
        &SystemRandom::new(),
    )
    .map_err(|e| anyhow!("the {name} key is not valid: {e}"))?;
    let derived = Writer::new()
        .string(name.as_bytes())
        .string(curve.as_bytes())
        .string(pair.public_key().as_ref())
        .finish();
    Ok((Signer::Ecdsa { name, pair }, derived))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::UnparsedPublicKey;
    use std::process::Command;

    /// A key `ssh-keygen` made for one test.
    pub struct Generated {
        pub private: String,
        /// Its line of `authorized_keys`, comment included.
        pub public: String,
        pub fingerprint: String,
    }

    /// Make a key with `ssh-keygen`, unencrypted unless given a passphrase,
    /// with `extra` options (`-m PEM`).
    pub fn keygen(kind: &str, bits: &str, passphrase: &str, extra: &[&str]) -> Generated {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        let made = Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                kind,
                "-b",
                bits,
                "-N",
                passphrase,
                "-C",
                "aurcache-test",
            ])
            .args(extra)
            .arg("-f")
            .arg(&path)
            .status()
            .expect("ssh-keygen is needed to make the test keys");
        assert!(made.success());
        let listed = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(path.with_extension("pub"))
            .output()
            .unwrap();
        let listed = String::from_utf8(listed.stdout).unwrap();
        Generated {
            private: std::fs::read_to_string(&path).unwrap(),
            public: std::fs::read_to_string(path.with_extension("pub")).unwrap(),
            fingerprint: listed.split_whitespace().nth(1).unwrap().to_string(),
        }
    }

    pub fn ed25519_key() -> Generated {
        keygen("ed25519", "256", "", &[])
    }

    pub fn rsa_key() -> Generated {
        keygen("rsa", "3072", "", &[])
    }

    pub fn ecdsa_key() -> Generated {
        keygen("ecdsa", "256", "", &[])
    }

    /// The blob of a signature: its algorithm's name and its bytes.
    fn signature(blob: &[u8]) -> (String, Vec<u8>) {
        let mut reader = Reader::new(blob);
        let name = String::from_utf8(reader.string().unwrap().to_vec()).unwrap();
        let bytes = reader.string().unwrap().to_vec();
        assert!(reader.is_empty());
        (name, bytes)
    }

    /// The parts of a public key blob after its type.
    fn public_parts(blob: &[u8]) -> Vec<Vec<u8>> {
        let mut reader = Reader::new(blob);
        reader.string().unwrap();
        let mut parts = Vec::new();
        while !reader.is_empty() {
            parts.push(reader.string().unwrap().to_vec());
        }
        parts
    }

    fn same_public(key: &SshKey, line: &str) {
        let expected = line
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(key.authorized_key(), expected);
    }

    #[test]
    fn an_ed25519_key_signs_what_its_public_half_verifies() {
        let made = ed25519_key();
        let key = SshKey::parse(made.private.as_bytes()).unwrap();
        same_public(&key, &made.public);
        assert_eq!(key.comment(), "aurcache-test");
        let (name, signed) = signature(&key.sign(b"data", None).unwrap());
        assert_eq!(name, "ssh-ed25519");
        let public = &public_parts(key.public())[0];
        UnparsedPublicKey::new(&signature::ED25519, public)
            .verify(b"data", &signed)
            .unwrap();
    }

    #[test]
    fn an_rsa_key_signs_with_sha2_and_never_sha1() {
        let made = rsa_key();
        let key = SshKey::parse(made.private.as_bytes()).unwrap();
        same_public(&key, &made.public);
        let mut reader = Reader::new(key.public());
        reader.string().unwrap();
        let e = reader.mpint().unwrap().to_vec();
        let n = reader.mpint().unwrap().to_vec();
        let public = ring::rsa::PublicKeyComponents { n: &n, e: &e };
        for (hash, name, verify) in [
            (
                RsaHash::Sha256,
                "rsa-sha2-256",
                &signature::RSA_PKCS1_2048_8192_SHA256,
            ),
            (
                RsaHash::Sha512,
                "rsa-sha2-512",
                &signature::RSA_PKCS1_2048_8192_SHA512,
            ),
        ] {
            let (got, signed) = signature(&key.sign(b"data", Some(hash)).unwrap());
            assert_eq!(got, name);
            public.verify(verify, b"data", &signed).unwrap();
        }
        assert_eq!(key.sign(b"data", None), Err(Unsigned::RsaSha1));
    }

    #[test]
    fn an_ecdsa_key_signs_as_ssh_encodes_it() {
        let made = ecdsa_key();
        let key = SshKey::parse(made.private.as_bytes()).unwrap();
        same_public(&key, &made.public);
        let (name, inner) = signature(&key.sign(b"data", None).unwrap());
        assert_eq!(name, "ecdsa-sha2-nistp256");
        let mut reader = Reader::new(&inner);
        let (r, s) = (reader.mpint().unwrap(), reader.mpint().unwrap());
        let mut fixed = vec![0; 64];
        fixed[32 - r.len()..32].copy_from_slice(r);
        fixed[64 - s.len()..].copy_from_slice(s);
        let point = &public_parts(key.public())[1];
        UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point)
            .verify(b"data", &fixed)
            .unwrap();
    }

    #[test]
    fn the_fingerprint_is_the_one_ssh_keygen_prints() {
        for made in [ed25519_key(), rsa_key(), ecdsa_key()] {
            let key = SshKey::parse(made.private.as_bytes()).unwrap();
            assert_eq!(key.fingerprint(), made.fingerprint);
        }
    }

    #[test]
    fn a_key_the_worker_cannot_use_says_why() {
        let said = |text: &str| format!("{:#}", SshKey::parse(text.as_bytes()).err().unwrap());
        let encrypted = keygen("ed25519", "256", "secret", &[]).private;
        assert!(
            said(&encrypted).contains("passphrase"),
            "{}",
            said(&encrypted)
        );
        let pem = keygen("rsa", "2048", "", &["-m", "PEM"]).private;
        assert!(said(&pem).contains("OpenSSH format"), "{}", said(&pem));
        assert!(said("hello").contains("no private key"));
        let short = keygen("rsa", "1024", "", &[]).private;
        assert!(
            said(&short).contains("2048 to 4096 bits"),
            "{}",
            said(&short)
        );
        let key = ed25519_key().private;
        let mut lines: Vec<&str> = key.lines().collect();
        let corrupt = format!("!!{}", &lines[1][2..]);
        lines[1] = &corrupt;
        let broken = lines.join("\n");
        assert!(said(&broken).contains("base64"), "{}", said(&broken));
    }

    #[test]
    fn a_key_whose_halves_differ_is_refused() {
        let decoded = STANDARD
            .decode(
                ed25519_key()
                    .private
                    .lines()
                    .filter(|line| !line.starts_with("-----"))
                    .collect::<String>(),
            )
            .unwrap();
        // The public key in the header comes first after the counts; flip a
        // bit of it.
        let mut tampered = decoded;
        let at = MAGIC.len() + 28 + 4 + ED25519.len() + 4 + 5;
        tampered[at] ^= 1;
        let said = format!("{:#}", SshKey::decode(&tampered).err().unwrap());
        assert!(said.contains("does not match"), "{said}");
    }
}
