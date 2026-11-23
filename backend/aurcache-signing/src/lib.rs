//! The repository's signing key.
//!
//! One OpenPGP key per instance signs every package the repository publishes
//! and both of its databases, so a client trusts one key, once. It is built to
//! be a root: an Ed25519 primary key that only certifies, and an Ed25519
//! subkey under it that signs. Clients trust the primary key.
//!
//! The server signs with one component key of it, chosen once, when the key
//! is loaded: its one current signing subkey, or else its primary key if that
//! signs. A key with several current signing subkeys is refused: moving
//! from one signing subkey to the next while the server runs is not
//! supported. One that stops signing within [`EXPIRY_WARNING_DAYS`] loads,
//! and says so ([`InstanceKey::expiring`]).
//!
//! The key is a file, like the worker CA's, never a database row: the
//! database is what gets exported, backed up elsewhere and inspected. Nothing
//! here hands the secret key out, and its `Debug` shows the fingerprint only.

use anyhow::{Context, anyhow, bail};
use aurcache_common::fs::REPO_ROOT;
use aurcache_common::server_env;
use aurcache_common::settings::parse_boolean;
use pgp::composed::{
    ArmorOptions, Deserializable, DetachedSignature, KeyType, SecretKeyParamsBuilder,
    SignedPublicKey, SignedSecretKey, SignedSecretSubKey, SubkeyParamsBuilder,
};
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::public_key::PublicKeyAlgorithm;
use pgp::packet::{Signature, SignatureType, SubpacketData};
use pgp::ser::Serialize;
use pgp::types::{
    Fingerprint, KeyDetails, KeyId, KeyVersion, Password, S2kParams, SecretParams, StringToKey,
    Timestamp,
};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

/// The key's file name in the server's CA directory, where it is kept unless
/// `AURCACHE_SIGNING_KEY_FILE` names another.
pub const KEY_FILE: &str = "signing-key.asc";

/// The key's user ID. Clients identify the key by its fingerprint; this is
/// what `pacman-key --list-keys` shows beside it.
const USER_ID: &str = "AURCache repository";

/// What signatures hash with: what GnuPG itself uses with an Ed25519 key.
const HASH: HashAlgorithm = HashAlgorithm::Sha512;

/// How many days before a key stops signing the server warns of it -- at
/// start, or while it runs -- so that an expiry is dealt with before it is reached rather than
/// found by every repository change failing.
pub const EXPIRY_WARNING_DAYS: u64 = 30;

const DAY_SECS: u64 = 24 * 60 * 60;

/// The instance key, unlocked and ready to sign.
pub struct InstanceKey {
    key: SignedSecretKey,
    passphrase: Password,
    /// The component key that signs, chosen when the key was loaded.
    signer: Signer,
    /// When `signer` stops signing, in seconds since the epoch: when it
    /// expires, or when its primary key does if that is sooner. `None` for
    /// never.
    expires: Option<u64>,
    fingerprint: String,
    public_key: String,
}

/// Which of the key's component keys makes signatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signer {
    Primary,
    /// The secret subkey at this index.
    Subkey(usize),
}

impl std::fmt::Debug for InstanceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceKey")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// The instance key the environment configures, or `None` when the
/// repository is not signed (`AURCACHE_SIGN` unset or false).
///
/// The key is `AURCACHE_SIGNING_KEY_FILE` when that is set, which is only ever
/// read; otherwise it is [`KEY_FILE`] in `ca_dir`, created the first time.
pub fn from_env(ca_dir: &Path) -> anyhow::Result<Option<InstanceKey>> {
    let sign = server_env::AURCACHE_SIGN;
    let on = match sign.get() {
        Ok(value) => parse_boolean(&value).map_err(|e| anyhow!("{sign}: {e}"))?,
        Err(_) => false,
    };
    if !on {
        for configured in [
            server_env::AURCACHE_SIGNING_KEY_FILE,
            server_env::AURCACHE_SIGNING_KEY_PASSPHRASE_FILE,
        ] {
            if configured.get_os().is_some() {
                tracing::warn!(
                    "{configured} is set, but {sign} is not on: the repository is not signed"
                );
            }
        }
        return Ok(None);
    }
    let config = KeyConfig::from_env(ca_dir);
    let passphrase = config.passphrase()?;
    let key = match &config.key {
        KeyFile::Supplied(path) => InstanceKey::load(path, passphrase.as_deref()),
        KeyFile::Own(path) => InstanceKey::load_or_create(path, passphrase.as_deref()),
    }?;
    let path = config.key.path();
    if is_inside(path, Path::new(REPO_ROOT)) {
        tracing::warn!(
            "The repository signing key {} is inside the repository directory {REPO_ROOT}: \
             whatever else serves, mirrors or syncs that directory hands it out with the \
             packages. Keep it elsewhere: the CA directory on a volume of its own \
             ({}), or a key supplied with {}",
            path.display(),
            server_env::AURCACHE_CA_DIR,
            server_env::AURCACHE_SIGNING_KEY_FILE,
        );
    }
    Ok(Some(key))
}

/// Where the instance key is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyFile {
    /// The server's own: [`KEY_FILE`] in its CA directory, created there the
    /// first time.
    Own(PathBuf),
    /// One supplied with `AURCACHE_SIGNING_KEY_FILE`, which the server reads
    /// and never writes.
    Supplied(PathBuf),
}

impl KeyFile {
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Own(path) | Self::Supplied(path) => path,
        }
    }
}

/// Where the environment keeps the instance key and its passphrase, whether
/// or not the server signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyConfig {
    pub key: KeyFile,
    /// `AURCACHE_SIGNING_KEY_PASSPHRASE_FILE`, when it is set.
    pub passphrase_file: Option<PathBuf>,
}

impl KeyConfig {
    /// The configuration of a server whose CA directory is `ca_dir`.
    #[must_use]
    pub fn from_env(ca_dir: &Path) -> Self {
        Self {
            key: match server_env::AURCACHE_SIGNING_KEY_FILE.get_os() {
                Some(path) => KeyFile::Supplied(PathBuf::from(path)),
                None => KeyFile::Own(ca_dir.join(KEY_FILE)),
            },
            passphrase_file: server_env::AURCACHE_SIGNING_KEY_PASSPHRASE_FILE
                .get_os()
                .map(PathBuf::from),
        }
    }

    /// The passphrase in [`Self::passphrase_file`], when there is one.
    pub fn passphrase(&self) -> anyhow::Result<Option<String>> {
        self.passphrase_file
            .as_deref()
            .map(read_passphrase)
            .transpose()
    }
}

/// Whether `path` is inside `dir`, both followed through symlinks where
/// they exist.
fn is_inside(path: &Path, dir: &Path) -> bool {
    let resolve = |path: &Path| {
        std::fs::canonicalize(path)
            .or_else(|_| std::path::absolute(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    resolve(path).starts_with(resolve(dir))
}

/// What a key is refused with when rPGP refuses its checksum: see
/// [`parse_secret`].
const REFUSED_CHECKSUM: &str = "rPGP, which the server reads keys with, refuses this key's \
     checksum. Either GnuPG wrote this unprotected key's secret in a form the OpenPGP \
     standard does not allow, which rPGP refuses (https://github.com/rpgp/rpgp/issues/901), \
     or the key file is damaged. Lock the key with a passphrase (`gpg --passwd \
     <fingerprint>`), export it again and give the server that passphrase \
     (AURCACHE_SIGNING_KEY_PASSPHRASE_FILE). If that copy is refused too, restore the key \
     from a backup";

/// A secret key, armored or binary.
///
/// GnuPG writes about one unprotected Ed25519 secret in 256 -- one whose
/// first byte is zero -- as an MPI that keeps that leading zero octet, which
/// the OpenPGP standard (RFC 9580, and RFC 4880 before it) forbids; GnuPG
/// follows its own LibrePGP draft. rPGP refuses such a secret as an invalid
/// checksum; an rPGP contributor argues it should not accept one, and the
/// issue is open (<https://github.com/rpgp/rpgp/issues/901>). A damaged key
/// file is refused the same way, so the refusal names both, and what to try
/// for each.
fn parse_secret(bytes: &[u8]) -> anyhow::Result<SignedSecretKey> {
    let armored = std::str::from_utf8(bytes)
        .is_ok_and(|text| text.trim_start().starts_with("-----BEGIN PGP"));
    let parsed = if armored {
        SignedSecretKey::from_armor_single(bytes).map(|(key, _headers)| key)
    } else {
        SignedSecretKey::from_bytes(bytes)
    };
    match parsed {
        Ok(key) => Ok(key),
        Err(error) if format!("{error:#}").contains("Invalid checksum") => {
            bail!(REFUSED_CHECKSUM)
        }
        Err(error) => Err(anyhow::Error::new(error).context("not an OpenPGP secret key")),
    }
}

/// The passphrase in `path`, without the newline an editor or `echo` leaves
/// at its end.
fn read_passphrase(path: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the signing key passphrase {}", path.display()))?;
    let passphrase = text.strip_suffix('\n').map_or(text.as_str(), |line| {
        line.strip_suffix('\r').unwrap_or(line)
    });
    if passphrase.is_empty() {
        bail!("the signing key passphrase {} is empty", path.display());
    }
    Ok(passphrase.to_string())
}

impl InstanceKey {
    /// A new key: an Ed25519 primary key that certifies, and an Ed25519
    /// subkey that signs. Both are locked with `passphrase` when there is one.
    pub fn generate(passphrase: Option<&str>) -> anyhow::Result<Self> {
        let mut subkey = SubkeyParamsBuilder::default();
        subkey
            .version(KeyVersion::V4)
            .key_type(KeyType::Ed25519Legacy)
            .can_sign(true)
            .passphrase(passphrase.map(str::to_string));
        let mut params = SecretKeyParamsBuilder::default();
        params
            .version(KeyVersion::V4)
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(false)
            .primary_user_id(USER_ID.to_string())
            .passphrase(passphrase.map(str::to_string))
            .subkeys(vec![subkey.build()?]);
        let key = params
            .build()?
            .generate(rand::thread_rng())
            .context("generating the signing key")?;
        Self::new(key, passphrase.map_or_else(Password::empty, Password::from))
    }

    /// The key in `path`, armored or binary, unlocked with `passphrase`.
    ///
    /// Refused unless it signs: a key that does not unlock, or has expired,
    /// fails here rather than at a publish.
    pub fn load(path: &Path, passphrase: Option<&str>) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading the signing key {}", path.display()))?;
        let key = parse_secret(&bytes)
            .with_context(|| format!("reading the signing key {}", path.display()))?;
        if passphrase.is_some() && !key.primary_key.secret_params().is_encrypted() {
            tracing::warn!(
                "the signing key {} is not protected by a passphrase; the one configured is not used to unlock it",
                path.display()
            );
        }
        Self::new(key, passphrase.map_or_else(Password::empty, Password::from))
            .with_context(|| format!("using the signing key {}", path.display()))
    }

    /// The key in `path`, or a new one written there (owner-only) when there
    /// is none yet.
    pub fn load_or_create(path: &Path, passphrase: Option<&str>) -> anyhow::Result<Self> {
        match std::fs::metadata(path) {
            Ok(_) => Self::load(path, passphrase),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate(passphrase)?;
                key.persist(path)?;
                tracing::info!(
                    "Created the repository signing key {} at {}",
                    key.fingerprint,
                    path.display()
                );
                Ok(key)
            }
            Err(error) => Err(anyhow::Error::new(error)
                .context(format!("reading the signing key {}", path.display()))),
        }
    }

    fn new(key: SignedSecretKey, passphrase: Password) -> anyhow::Result<Self> {
        let key = self_signed(key);
        key.verify_bindings()
            .context("the key's self-signatures do not verify")?;
        let Choice { signer, expires } = choose(&key, now()).map_err(|why| refusal(&key, why))?;
        if is_rsa(signing_algorithm(&key, signer)) {
            tracing::warn!(
                "the repository's signing {} is an RSA key: it signs, but through an RSA \
                 implementation with a known timing side channel (RUSTSEC-2023-0071); an \
                 Ed25519 key, which the server generates, has none",
                describe(&key, signer)
            );
        }
        let fingerprint = format!("{:X}", key.primary_key.fingerprint());
        let public_key = SignedPublicKey::from(key.clone())
            .to_armored_string(ArmorOptions::default())
            .context("armoring the public key")?;
        let key = Self {
            key,
            passphrase,
            signer,
            expires,
            fingerprint,
            public_key,
        };
        let probe = key
            .sign(&b""[..])
            .context("the key does not sign; is its passphrase right?")?;
        if !key.verifies(&probe, b"") {
            bail!("a signature the key made does not verify");
        }
        if let Some(expiring) = key.expiring() {
            tracing::warn!("{expiring}");
        } else if let Some(at) = expires {
            tracing::info!(
                "the repository's signing {} stops signing on {}; the server warns \
                 {EXPIRY_WARNING_DAYS} days before",
                describe(&key.key, signer),
                date(at)
            );
        }
        Ok(key)
    }

    /// When the component key that signs stops signing within
    /// [`EXPIRY_WARNING_DAYS`]: it signs until then, and every repository
    /// change fails after.
    #[must_use]
    pub fn expiring(&self) -> Option<Expiring> {
        let at = self
            .expires
            .filter(|at| *at < now() + EXPIRY_WARNING_DAYS * DAY_SECS)?;
        let by_primary = self.signer != Signer::Primary
            && primary_expiry(&self.key, now()).ok().flatten() == Some(at);
        Some(Expiring {
            signer: describe(&self.key, self.signer),
            on: date(at),
            primary: by_primary.then(|| describe(&self.key, Signer::Primary)),
        })
    }

    /// How long until [`Self::expiring`] says the key stops signing soon:
    /// `None` when it says so already, or the key never stops signing.
    #[must_use]
    pub fn warns_in(&self) -> Option<std::time::Duration> {
        warns_in(self.expires, now())
    }

    fn persist(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let armored = self
            .key
            .to_armored_string(ArmorOptions::default())
            .context("armoring the signing key")?;
        aurcache_common::fs::write_private(path, armored.as_bytes())
            .with_context(|| format!("writing the signing key {}", path.display()))
    }

    /// The primary key's fingerprint, as `pacman-key --lsign-key` takes it:
    /// 40 uppercase hex digits.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The public key, armored, as `pacman-key --add` takes it.
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// A detached binary signature over `data`, as pacman reads one from a
    /// `.sig` file or from `%PGPSIG%`. Refused once the component key that
    /// signs has stopped signing: clients would refuse the signature.
    pub fn sign(&self, data: impl Read) -> anyhow::Result<Vec<u8>> {
        if let Some(at) = self.expires.filter(|at| *at <= now()) {
            bail!(
                "the repository's signing {} stopped signing on {}: the server needs a key that \
                 signs, and a restart to load it",
                describe(&self.key, self.signer),
                date(at)
            );
        }
        let rng = rand::thread_rng();
        let data = BufReader::new(data);
        let signature = match self.signer {
            Signer::Primary => DetachedSignature::sign_binary_data(
                rng,
                &self.key.primary_key,
                &self.passphrase,
                HASH,
                data,
            ),
            Signer::Subkey(index) => DetachedSignature::sign_binary_data(
                rng,
                &self.key.secret_subkeys[index].key,
                &self.passphrase,
                HASH,
                data,
            ),
        }
        .context("signing")?;
        Ok(signature.to_bytes()?)
    }

    /// A detached signature over the file at `path`.
    pub fn sign_file(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        self.sign(file)
            .with_context(|| format!("signing {}", path.display()))
    }

    /// Whether this key made `signature`, as the issuer it names: the
    /// component key that signs, not merely another key under the same
    /// primary key. Says nothing about what it signs: see [`Self::verifies`].
    #[must_use]
    pub fn issued(&self, signature: &[u8]) -> bool {
        let Ok(parsed) = DetachedSignature::from_bytes(signature) else {
            return false;
        };
        let Issuer { fingerprint, id } = self.issuer();
        parsed
            .signature
            .issuer_fingerprint()
            .into_iter()
            .any(|named| *named == fingerprint)
            || parsed
                .signature
                .issuer_key_id()
                .into_iter()
                .any(|named| *named == id)
    }

    /// Whether `signature` is this key's signature over `content`.
    #[must_use]
    pub fn verifies(&self, signature: &[u8], content: &[u8]) -> bool {
        let Ok(parsed) = DetachedSignature::from_bytes(signature) else {
            return false;
        };
        if parsed.signature.typ() != Some(SignatureType::Binary) {
            return false;
        }
        match self.signer {
            Signer::Primary => parsed
                .verify(self.key.primary_key.public_key(), content)
                .is_ok(),
            Signer::Subkey(index) => parsed
                .verify(self.key.secret_subkeys[index].key.public_key(), content)
                .is_ok(),
        }
    }

    fn issuer(&self) -> Issuer {
        match self.signer {
            Signer::Primary => Issuer {
                fingerprint: self.key.primary_key.fingerprint(),
                id: self.key.primary_key.legacy_key_id(),
            },
            Signer::Subkey(index) => {
                let subkey = &self.key.secret_subkeys[index].key;
                Issuer {
                    fingerprint: subkey.fingerprint(),
                    id: subkey.legacy_key_id(),
                }
            }
        }
    }
}

/// A signing key that stops signing soon, as [`InstanceKey::expiring`]
/// says it. Its `Display` is the warning an operator reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expiring {
    /// The component key that signs, as `gpg --list-keys` shows it:
    /// `subkey <fingerprint>` or `primary key <fingerprint>`.
    pub signer: String,
    /// The day it stops signing, `YYYY-MM-DD` in UTC.
    pub on: String,
    /// The primary key, named as `signer` is, when it is the primary key's
    /// expiry that ends the subkey's signing.
    pub primary: Option<String>,
}

impl std::fmt::Display for Expiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            signer,
            on,
            primary,
        } = self;
        let because = primary.as_ref().map_or_else(String::new, |primary| {
            format!(", when its {primary} expires")
        });
        // The primary key's expiry, when it is what ends the signing, is the
        // one to extend.
        let extend = primary.as_deref().unwrap_or("it");
        write!(
            f,
            "the repository's signing {signer} stops signing on {on}{because}, and every \
             repository change fails from then on: extend {extend} (`gpg --quick-set-expire`), \
             restart the server with the extended key, then have every client import it again \
             from /api/repo/key before {on}"
        )
    }
}

/// Why a key is refused, when its primary key is current but nothing in it
/// signs.
const NOTHING_SIGNS: &str = "nothing in the signing key signs now: no subkey that signs \
     is current (neither expired nor revoked) with its secret in the file, and the primary \
     key does not sign either";

/// Why a key is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// Its primary key has expired, at this time in seconds since the epoch,
    /// and every subkey's signatures with it.
    PrimaryExpired { at: u64 },
    /// Its primary key is revoked, and every subkey's signatures with it.
    PrimaryRevoked,
    /// Its primary key is current, but no subkey signs and is current with
    /// its secret in the file, and the primary key does not sign either.
    NoSigner,
    /// More than one subkey signs and is current, with its secret in the
    /// file: the server signs with one.
    SeveralSigners { count: usize },
}

/// The error a key is refused with, for `why`.
fn refusal(key: &SignedSecretKey, why: Refusal) -> anyhow::Error {
    let primary = key.primary_key.fingerprint();
    match why {
        Refusal::PrimaryExpired { at } => anyhow!(
            "the signing key's primary key {primary:X} expired on {}, and pacman refuses every \
             signature under an expired primary key, its subkeys' included: extend it \
             (`gpg --quick-set-expire {primary:X} <expiry>`), then give the server the key \
             again and have every client import it again",
            date(at)
        ),
        Refusal::PrimaryRevoked => anyhow!(
            "the signing key's primary key {primary:X} is revoked, and pacman refuses every \
             signature under a revoked primary key, its subkeys' included: the server needs \
             another key"
        ),
        Refusal::NoSigner => anyhow!(NOTHING_SIGNS),
        Refusal::SeveralSigners { count } => anyhow!(
            "the signing key has {count} current signing subkeys, and the server signs with \
             one: export the key with only the subkey to sign with \
             (`gpg --export-secret-subkeys <subkey fingerprint>!`, the `!` included)"
        ),
    }
}

/// A time in seconds since the epoch, as a UTC date.
fn date(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|secs| jiff::Timestamp::from_second(secs).ok())
        .map_or_else(
            || secs.to_string(),
            |at| at.strftime("%Y-%m-%d").to_string(),
        )
}

/// How long after `now` a key that stops signing at `expires` is within
/// [`EXPIRY_WARNING_DAYS`] of it: a second past the moment, so that
/// [`InstanceKey::expiring`] says so then. `None` when it is already.
fn warns_in(expires: Option<u64>, now: u64) -> Option<std::time::Duration> {
    let from = expires?.checked_sub(EXPIRY_WARNING_DAYS * DAY_SECS)?;
    (from >= now).then(|| std::time::Duration::from_secs(from - now + 1))
}

/// The current time, in seconds since the epoch, as key times are kept.
fn now() -> u64 {
    u64::from(Timestamp::now().as_secs())
}

/// How a signature names the key that made it.
struct Issuer {
    fingerprint: Fingerprint,
    id: KeyId,
}

/// The component key that signs, and until when it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Choice {
    signer: Signer,
    /// When it stops signing, in seconds since the epoch: when it expires, or
    /// when its primary key does if that is sooner. `None` for never.
    expires: Option<u64>,
}

/// The sooner of two expiries, `None` being never.
fn sooner(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The component key of `key` that signs at `now`, as GnuPG would pick it:
/// its one current signing subkey -- what a key this server generated has --
/// else its primary key if that signs. A subkey signs only while its primary
/// key is in force: GnuPG, and pacman through it, takes a subkey's signature
/// under an expired primary key for an expired signature, and under a
/// revoked one for a revoked one, whatever the subkey's own binding says.
fn choose(key: &SignedSecretKey, now: u64) -> Result<Choice, Refusal> {
    let primary = primary_expiry(key, now)?;
    let current: Vec<Choice> = key
        .secret_subkeys
        .iter()
        .enumerate()
        .filter_map(|(index, subkey)| {
            subkey_signs(subkey, now).map(|expires| Choice {
                signer: Signer::Subkey(index),
                expires,
            })
        })
        .collect();
    let (signer, own) = match current.as_slice() {
        [] if primary_signs(key) => (Signer::Primary, None),
        [] => return Err(Refusal::NoSigner),
        [one] => (one.signer, one.expires),
        several => {
            return Err(Refusal::SeveralSigners {
                count: several.len(),
            });
        }
    };
    Ok(Choice {
        signer,
        expires: sooner(own, primary),
    })
}

/// When `subkey` stops signing by its own signatures (seconds since the
/// epoch, `None` for never), when it signs at `now`: its latest binding says
/// it signs and it has not expired, nothing revokes it, and its secret is in
/// the file. Its primary key can end that sooner ([`primary_expiry`]).
fn subkey_signs(subkey: &SignedSecretSubKey, now: u64) -> Option<Option<u64>> {
    let revoked = subkey
        .signatures
        .iter()
        .any(|sig| sig.typ() == Some(SignatureType::SubkeyRevocation));
    let binding = subkey
        .signatures
        .iter()
        .filter(|sig| sig.typ() == Some(SignatureType::SubkeyBinding))
        .max_by_key(|sig| sig.created().map_or(0, Timestamp::as_secs))?;
    if revoked || !binding.key_flags().sign() || !holds_secret(subkey.key.secret_params()) {
        return None;
    }
    let created = u64::from(subkey.key.created_at().as_secs());
    match expiry(created, binding) {
        Some(at) if at <= now => None,
        expires => Some(expires),
    }
}

/// When `key`'s primary key expires (seconds since the epoch, `None` for
/// never), by its newest self-signature, if it is in force at `now`: neither
/// revoked by a revocation it made nor expired.
fn primary_expiry(key: &SignedSecretKey, now: u64) -> Result<Option<u64>, Refusal> {
    let revoked = key
        .details
        .revocation_signatures
        .iter()
        .any(|sig| sig.typ() == Some(SignatureType::KeyRevocation));
    if revoked {
        return Err(Refusal::PrimaryRevoked);
    }
    let created = u64::from(key.primary_key.created_at().as_secs());
    match newest_self_signature(key).and_then(|sig| expiry(created, sig)) {
        Some(at) if at <= now => Err(Refusal::PrimaryExpired { at }),
        expires => Ok(expires),
    }
}

/// Whether the primary key signs itself, as [`subkey_signs`] says of a
/// subkey, once [`primary_expiry`] says it is in force: its secret is in the
/// file, and its newest self-signature lets it sign.
fn primary_signs(key: &SignedSecretKey) -> bool {
    if !holds_secret(key.primary_key.secret_params()) {
        return false;
    }
    // Without key flags, what the key does is its algorithm's to say; the
    // probe signature at load tells a key that cannot sign.
    newest_self_signature(key)
        .is_some_and(|newest| !states_key_flags(newest) || newest.key_flags().sign())
}

/// The primary key's newest self-signature that says what it does and until
/// when: a direct-key signature or a certification of one of its user IDs.
/// Revocations, which say neither, are not among them.
fn newest_self_signature(key: &SignedSecretKey) -> Option<&Signature> {
    let fingerprint = key.primary_key.fingerprint();
    let id = key.primary_key.legacy_key_id();
    key.details
        .direct_signatures
        .iter()
        .chain(key.details.users.iter().flat_map(|user| &user.signatures))
        .filter(|sig| {
            matches!(
                sig.typ(),
                Some(
                    SignatureType::CertGeneric
                        | SignatureType::CertPersona
                        | SignatureType::CertCasual
                        | SignatureType::CertPositive
                        | SignatureType::Key
                )
            )
        })
        .filter(|sig| {
            sig.issuer_fingerprint().contains(&&fingerprint) || sig.issuer_key_id().contains(&&id)
        })
        .max_by_key(|sig| sig.created().map_or(0, Timestamp::as_secs))
}

/// When a key created at `created` expires by its self-signature `sig`, in
/// seconds since the epoch; `None` for never.
fn expiry(created: u64, sig: &Signature) -> Option<u64> {
    sig.key_expiration_time()
        .map(|lifetime| u64::from(lifetime.as_secs()))
        .filter(|lifetime| *lifetime > 0)
        .map(|lifetime| created + lifetime)
}

/// `key` with only the signatures its own primary key made, which is what
/// checking it takes: a certification someone else made -- a colleague's,
/// which `gpg --export-secret-keys` includes -- says something about the key
/// rather than being part of it, and rPGP's `verify_bindings` refuses one
/// whose issuer it does not have. A user ID left with no signature goes too.
fn self_signed(mut key: SignedSecretKey) -> SignedSecretKey {
    let fingerprint = key.primary_key.fingerprint();
    let id = key.primary_key.legacy_key_id();
    let own = |sig: &Signature| {
        let fingerprints = sig.issuer_fingerprint();
        let ids = sig.issuer_key_id();
        // Naming no issuer, it can only be checked against the key itself.
        (fingerprints.is_empty() && ids.is_empty())
            || fingerprints.contains(&&fingerprint)
            || ids.contains(&&id)
    };
    let details = &mut key.details;
    details.direct_signatures.retain(own);
    details.revocation_signatures.retain(own);
    for user in &mut details.users {
        user.signatures.retain(own);
    }
    details.users.retain(|user| !user.signatures.is_empty());
    for attribute in &mut details.user_attributes {
        attribute.signatures.retain(own);
    }
    details
        .user_attributes
        .retain(|attribute| !attribute.signatures.is_empty());
    key
}

/// The algorithm of the component key `signer` of `key`.
fn signing_algorithm(key: &SignedSecretKey, signer: Signer) -> PublicKeyAlgorithm {
    match signer {
        Signer::Primary => key.primary_key.algorithm(),
        Signer::Subkey(index) => key.secret_subkeys[index].key.algorithm(),
    }
}

fn is_rsa(algorithm: PublicKeyAlgorithm) -> bool {
    matches!(
        algorithm,
        PublicKeyAlgorithm::RSA | PublicKeyAlgorithm::RSASign
    )
}

/// Whether `sig` carries a key flags subpacket at all.
fn states_key_flags(sig: &Signature) -> bool {
    sig.config().is_some_and(|config| {
        config
            .hashed_subpackets()
            .any(|packet| matches!(packet.data, SubpacketData::KeyFlags(_)))
    })
}

/// Whether `params` holds a secret this server can sign with: not one of
/// GnuPG's stubs (S2K type 101), which `gpg --export-secret-subkeys` leaves
/// for a primary key kept offline, and a smartcard for a key on the card.
fn holds_secret(params: &SecretParams) -> bool {
    let SecretParams::Encrypted(encrypted) = params else {
        return true;
    };
    let s2k = match encrypted.string_to_key_params() {
        S2kParams::Cfb { s2k, .. }
        | S2kParams::MalleableCfb { s2k, .. }
        | S2kParams::Aead { s2k, .. } => s2k,
        S2kParams::Unprotected | S2kParams::LegacyCfb { .. } => return true,
    };
    !matches!(s2k, StringToKey::Private { typ: 101, .. })
}

/// A component key as an operator finds it in `gpg --list-keys`.
fn describe(key: &SignedSecretKey, signer: Signer) -> String {
    match signer {
        Signer::Primary => format!("primary key {:X}", key.primary_key.fingerprint()),
        Signer::Subkey(index) => {
            format!("subkey {:X}", key.secret_subkeys[index].key.fingerprint())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// What a package or a database looks like to the key: bytes.
    const CONTENT: &[u8] = b"pkgname = hello\npkgver = 1.0-1\n";

    #[test]
    fn a_signature_verifies_over_what_it_signed_and_nothing_else() {
        let key = InstanceKey::generate(None).unwrap();
        let signature = key.sign(CONTENT).unwrap();

        assert!(key.verifies(&signature, CONTENT));
        assert!(!key.verifies(&signature, b"pkgname = hello\npkgver = 1.0-2\n"));
        assert!(key.issued(&signature));
        assert!(!key.issued(b"not a signature"));
    }

    #[test]
    fn another_key_did_not_issue_it() {
        let ours = InstanceKey::generate(None).unwrap();
        let theirs = InstanceKey::generate(None).unwrap();
        let signature = theirs.sign(CONTENT).unwrap();

        assert!(!ours.issued(&signature));
        assert!(!ours.verifies(&signature, CONTENT));
    }

    /// A generated key signs with its subkey, so that the primary key clients
    /// trust only ever certifies.
    #[test]
    fn a_generated_key_signs_with_its_subkey() {
        let key = InstanceKey::generate(None).unwrap();
        assert_eq!(key.signer, Signer::Subkey(0));
        assert_eq!(key.expires, None);
        assert_eq!(key.fingerprint().len(), 40);
        assert!(
            key.fingerprint()
                .chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        );
        assert!(
            key.public_key()
                .starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----")
        );
    }

    /// The key a server creates is the one it loads next time, readable by
    /// its owner alone.
    #[test]
    fn a_created_key_is_kept_and_loaded_again() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ca").join(KEY_FILE);
        let created = InstanceKey::load_or_create(&path, None).unwrap();
        let loaded = InstanceKey::load_or_create(&path, None).unwrap();

        assert_eq!(created.fingerprint(), loaded.fingerprint());
        let signature = loaded.sign(CONTENT).unwrap();
        assert!(created.verifies(&signature, CONTENT));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_passphrase_locks_the_key_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(KEY_FILE);
        let created = InstanceKey::load_or_create(&path, Some("correct horse")).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        let (parsed, _) = SignedSecretKey::from_string(&on_disk).unwrap();
        assert!(parsed.primary_key.secret_params().is_encrypted());
        assert!(parsed.secret_subkeys[0].key.secret_params().is_encrypted());

        assert!(InstanceKey::load(&path, None).is_err());
        assert!(InstanceKey::load(&path, Some("wrong horse")).is_err());
        let loaded = InstanceKey::load(&path, Some("correct horse")).unwrap();
        assert_eq!(loaded.fingerprint(), created.fingerprint());
        assert!(created.verifies(&loaded.sign(CONTENT).unwrap(), CONTENT));
    }

    #[test]
    fn a_passphrase_file_loses_its_final_newline_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("passphrase");
        for (written, read) in [
            ("secret\n", "secret"),
            ("secret\r\n", "secret"),
            ("secret", "secret"),
            ("two words \n\n", "two words \n"),
        ] {
            std::fs::write(&path, written).unwrap();
            assert_eq!(read_passphrase(&path).unwrap(), read);
        }
        std::fs::write(&path, "\n").unwrap();
        assert!(read_passphrase(&path).is_err());
    }

    #[test]
    fn a_file_that_is_not_a_key_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(KEY_FILE);
        std::fs::write(&path, "not a key").unwrap();
        assert!(InstanceKey::load_or_create(&path, None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a key");
    }

    /// The key's debug form, which ends up in logs, never carries the secret.
    #[test]
    fn debug_shows_the_fingerprint_only() {
        let key = InstanceKey::generate(None).unwrap();
        let shown = format!("{key:?}");
        assert!(shown.contains(key.fingerprint()));
        assert!(!shown.contains("secret"));
        assert!(shown.len() < 100);
    }

    /// GnuPG, as pacman verifies through it, accepts what this key signs --
    /// with the trust pacman-key sets up: a local signature on the primary key
    /// from an ultimately trusted key gives the subkey's signatures full
    /// validity.
    #[test]
    fn gnupg_accepts_the_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(gpg) = Gpg::new(tmp.path()) else {
            eprintln!("skipped: gpg is not installed");
            return;
        };
        let key = InstanceKey::generate(None).unwrap();
        let public = tmp.path().join("key.asc");
        std::fs::write(&public, key.public_key()).unwrap();
        let content = tmp.path().join("hello-1.0-1-x86_64.pkg.tar.zst");
        std::fs::write(&content, CONTENT).unwrap();
        let signature = tmp.path().join("hello-1.0-1-x86_64.pkg.tar.zst.sig");
        std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
        let verify = [signature.to_str().unwrap(), content.to_str().unwrap()];

        let imported = gpg.run(&["--import", public.to_str().unwrap()]);
        assert!(imported.success, "gpg --import: {}", imported.stderr);

        let verified = gpg.verify(verify);
        assert!(verified.success, "gpg --verify: {}", verified.stderr);
        assert!(
            verified
                .status
                .lines()
                .any(|line| line.starts_with("[GNUPG:] VALIDSIG")
                    && line.ends_with(key.fingerprint())),
            "the signature is not a valid one from {}: {}",
            key.fingerprint(),
            verified.status
        );

        std::fs::write(&content, b"pkgname = hello\npkgver = 6.6.6-1\n").unwrap();
        let tampered = gpg.verify(verify);
        assert!(!tampered.success, "gpg accepted a tampered file");
        assert!(tampered.status.contains("[GNUPG:] BADSIG"));
        std::fs::write(&content, CONTENT).unwrap();

        // What `pacman-key --init` and `--lsign-key` do: a local master key,
        // ultimately trusted, locally signing the repository's primary key.
        let master = gpg.unattended(&[
            "--quick-gen-key",
            "Local master <master@localhost>",
            "ed25519",
            "cert",
            "never",
        ]);
        if !master.success {
            eprintln!(
                "skipped the trust check: gpg could not make a local key ({})",
                master.stderr
            );
            return;
        }
        let signed = gpg.unattended(&["--quick-lsign-key", key.fingerprint()]);
        assert!(signed.success, "gpg --quick-lsign-key: {}", signed.stderr);
        let trusted = gpg.verify(verify);
        assert!(trusted.success, "gpg --verify: {}", trusted.stderr);
        assert!(
            trusted.status.contains("[GNUPG:] TRUST_FULLY")
                || trusted.status.contains("[GNUPG:] TRUST_ULTIMATE"),
            "a subkey signature is not fully valid through the lsigned primary key: {}",
            trusted.status
        );
    }

    /// A key made and exported by GnuPG, as an operator would supply one,
    /// loads -- locked with a passphrase or not -- and signs what GnuPG then
    /// verifies.
    #[test]
    fn a_key_gnupg_exported_signs() {
        with_gnupg(|tmp, gpg| {
            for passphrase in ["", "correct horse"] {
                let user = format!("Operator {} <ops@localhost>", passphrase.len());
                let made = gpg.run(&[
                    "--pinentry-mode",
                    "loopback",
                    "--passphrase",
                    passphrase,
                    "--quick-gen-key",
                    &user,
                    "ed25519",
                    "sign",
                    "never",
                ]);
                if !made.success {
                    eprintln!("skipped: gpg could not make a key ({})", made.stderr);
                    return;
                }
                let exported = Command::new("gpg")
                    .arg("--homedir")
                    .arg(&gpg.home)
                    .args(["--batch", "--pinentry-mode", "loopback", "--passphrase"])
                    .arg(passphrase)
                    .args(["--armor", "--export-secret-keys", &user])
                    .output()
                    .unwrap();
                assert!(exported.status.success());
                let path = tmp.join(format!("supplied-{}.asc", passphrase.len()));
                std::fs::write(&path, &exported.stdout).unwrap();

                let key =
                    InstanceKey::load(&path, Some(passphrase).filter(|p| !p.is_empty())).unwrap();
                let content = tmp.join("content");
                std::fs::write(&content, CONTENT).unwrap();
                let signature = tmp.join("content.sig");
                std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
                let verified = gpg.verify([signature.to_str().unwrap(), content.to_str().unwrap()]);
                assert!(verified.success, "gpg --verify: {}", verified.stderr);
                assert!(
                    verified.status.contains(key.fingerprint()),
                    "{}",
                    verified.status
                );
            }
        });
    }

    /// A supplied key whose first signing subkey has expired -- as GnuPG
    /// keys do, by default -- signs with its one current one, whose
    /// signatures GnuPG accepts.
    #[test]
    fn an_expired_subkey_is_passed_over() {
        with_gnupg(|tmp, gpg| {
            let user = "Rotated <ops@localhost>";
            let made = gpg.unattended(&[
                "--faked-system-time",
                "20200101T000000",
                "--quick-gen-key",
                user,
                "ed25519",
                "cert",
                "never",
            ]);
            if !made.success {
                eprintln!("skipped: gpg could not make a key ({})", made.stderr);
                return;
            }
            let primary = made
                .status
                .lines()
                .find_map(|line| line.strip_prefix("[GNUPG:] KEY_CREATED P "))
                .expect("gpg names the key it made")
                .trim()
                .to_string();
            // Made in 2020, for a day: long expired.
            let expired = gpg.run(&[
                "--faked-system-time",
                "20200102T000000",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--quick-add-key",
                &primary,
                "ed25519",
                "sign",
                "1d",
            ]);
            assert!(expired.success, "gpg --quick-add-key: {}", expired.stderr);
            let current =
                gpg.unattended(&["--quick-add-key", &primary, "ed25519", "sign", "never"]);
            assert!(current.success, "gpg --quick-add-key: {}", current.stderr);
            let exported = Command::new("gpg")
                .arg("--homedir")
                .arg(&gpg.home)
                .args(["--batch", "--pinentry-mode", "loopback", "--passphrase", ""])
                .args(["--armor", "--export-secret-keys", &primary])
                .output()
                .unwrap();
            assert!(exported.status.success());
            let path = tmp.join("rotated.asc");
            std::fs::write(&path, &exported.stdout).unwrap();

            let key = InstanceKey::load(&path, None).unwrap();
            assert_eq!(key.signer, Signer::Subkey(1));
            let content = tmp.join("content");
            std::fs::write(&content, CONTENT).unwrap();
            let signature = tmp.join("content.sig");
            std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
            let verified = gpg.verify([signature.to_str().unwrap(), content.to_str().unwrap()]);
            assert!(verified.success, "gpg --verify: {}", verified.stderr);
            assert!(
                verified.status.contains("[GNUPG:] GOODSIG")
                    && !verified.status.contains("EXPKEYSIG"),
                "{}",
                verified.status
            );
        });
    }

    /// A key whose primary key stays offline -- `gpg --export-secret-subkeys`,
    /// which leaves a stub where its secret was -- signs with its subkey,
    /// locked or not; one whose only key that signs is that primary is
    /// refused for what it is, not for a passphrase.
    #[test]
    fn a_primary_key_kept_offline_is_not_needed() {
        with_gnupg(|tmp, gpg| {
            for passphrase in ["", "correct horse"] {
                let user = format!("Offline {} <ops@localhost>", passphrase.len());
                let Some(primary) = gpg.make(&user, "cert", passphrase) else {
                    return;
                };
                let added = gpg.with_passphrase(
                    passphrase,
                    &["--quick-add-key", &primary, "ed25519", "sign", "never"],
                );
                assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
                let path = tmp.join(format!("offline-{}.gpg", passphrase.len()));
                std::fs::write(
                    &path,
                    gpg.export("--export-secret-subkeys", &primary, passphrase),
                )
                .unwrap();

                let key =
                    InstanceKey::load(&path, Some(passphrase).filter(|p| !p.is_empty())).unwrap();
                assert!(!holds_secret(key.key.primary_key.secret_params()));
                assert_eq!(key.signer, Signer::Subkey(0));
                let content = tmp.join("content");
                std::fs::write(&content, CONTENT).unwrap();
                let signature = tmp.join("content.sig");
                std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
                let verified = gpg.verify([signature.to_str().unwrap(), content.to_str().unwrap()]);
                assert!(verified.success, "gpg --verify: {}", verified.stderr);
            }

            let Some(primary) = gpg.make("Signs offline <ops@localhost>", "sign", "") else {
                return;
            };
            let added = gpg.unattended(&["--quick-add-key", &primary, "cv25519", "encr", "never"]);
            assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
            let path = tmp.join("signs-offline.gpg");
            std::fs::write(&path, gpg.export("--export-secret-subkeys", &primary, "")).unwrap();
            let error = format!("{:#}", InstanceKey::load(&path, None).unwrap_err());
            assert!(error.contains(NOTHING_SIGNS), "{error}");
        });
    }

    /// A key with nothing that signs -- a primary key that only certifies, a
    /// subkey that only authenticates -- is refused when it is loaded.
    #[test]
    fn a_key_where_nothing_signs_is_refused() {
        let mut subkey = SubkeyParamsBuilder::default();
        subkey
            .version(KeyVersion::V4)
            .key_type(KeyType::Ed25519Legacy)
            .can_authenticate(true);
        let mut params = SecretKeyParamsBuilder::default();
        params
            .version(KeyVersion::V4)
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(false)
            .primary_user_id(USER_ID.to_string())
            .subkeys(vec![subkey.build().unwrap()]);
        let key = params
            .build()
            .unwrap()
            .generate(rand::thread_rng())
            .unwrap();
        let error = format!(
            "{:#}",
            InstanceKey::new(key, Password::empty()).unwrap_err()
        );
        assert!(error.contains(NOTHING_SIGNS), "{error}");
    }

    /// A key with two current signing subkeys is refused: the server signs
    /// with one. Exported with only one of them, as the refusal says, it
    /// loads and signs with that one.
    #[test]
    fn a_key_with_several_signing_subkeys_is_refused() {
        with_gnupg(|tmp, gpg| {
            let Some(primary) = gpg.make("Two signers <ops@localhost>", "cert", "") else {
                return;
            };
            let mut subkeys = Vec::new();
            for _ in 0..2 {
                let added =
                    gpg.unattended(&["--quick-add-key", &primary, "ed25519", "sign", "never"]);
                assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
                subkeys.push(
                    added
                        .created_subkey()
                        .expect("gpg names the subkey it made"),
                );
            }
            let path = tmp.join("two.gpg");
            std::fs::write(&path, gpg.export("--export-secret-keys", &primary, "")).unwrap();
            let error = format!("{:#}", InstanceKey::load(&path, None).unwrap_err());
            assert!(error.contains("has 2 current signing subkeys"), "{error}");

            let only = format!("{}!", subkeys[1]);
            let path = tmp.join("one.gpg");
            std::fs::write(&path, gpg.export("--export-secret-subkeys", &only, "")).unwrap();
            let key = InstanceKey::load(&path, None).unwrap();
            assert_eq!(key.fingerprint(), primary);
            assert_eq!(
                format!("{:X}", key.issuer().fingerprint),
                subkeys[1],
                "the subkey exported is the one that signs"
            );
        });
    }

    /// A key whose signing subkey stops signing within
    /// [`EXPIRY_WARNING_DAYS`] loads and says when; one with longer left
    /// loads without a word. Both sign until then.
    #[test]
    fn a_key_that_stops_signing_soon_loads_and_says_so() {
        with_gnupg(|tmp, gpg| {
            for (lifetime, days) in [("10d", 10), ("60d", 60)] {
                let user = format!("Expiring {days} <ops@localhost>");
                let Some(primary) = gpg.make(&user, "cert", "") else {
                    return;
                };
                // Before the subkey is made, so a slow machine cannot make it
                // look shorter-lived than it is.
                let before = now();
                let added =
                    gpg.unattended(&["--quick-add-key", &primary, "ed25519", "sign", lifetime]);
                assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
                let path = tmp.join(format!("expiring-{days}.gpg"));
                std::fs::write(&path, gpg.export("--export-secret-keys", &primary, "")).unwrap();
                let key = InstanceKey::load(&path, None).unwrap();
                let expires = key.expires.unwrap();
                assert!(
                    (before + days * DAY_SECS..=now() + days * DAY_SECS).contains(&expires),
                    "{expires}"
                );
                assert_eq!(key.signer, Signer::Subkey(0));
                if days < EXPIRY_WARNING_DAYS {
                    let expiring = key.expiring().expect("an expiry within the warning");
                    assert_eq!(expiring.on, date(expires));
                    assert!(expiring.signer.starts_with("subkey "), "{expiring}");
                    assert_eq!(expiring.primary, None, "{expiring}");
                } else {
                    assert_eq!(key.expiring(), None);
                }
            }
        });
    }

    /// The warning comes the moment a key is within [`EXPIRY_WARNING_DAYS`]
    /// of its expiry, and not again for one that is already.
    #[test]
    fn the_warning_waits_for_the_expiry_to_be_close() {
        let warning = EXPIRY_WARNING_DAYS * DAY_SECS;
        let at = 1_000 * DAY_SECS;
        assert_eq!(warns_in(None, at), None);
        assert_eq!(
            warns_in(Some(at + warning + 60), at),
            Some(std::time::Duration::from_secs(61))
        );
        assert_eq!(
            warns_in(Some(at + warning), at),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(warns_in(Some(at + warning - 1), at), None);
        assert_eq!(warns_in(Some(at - 1), at), None);
        assert_eq!(warns_in(Some(1), at), None);

        let key = InstanceKey::generate(None).unwrap();
        assert_eq!(key.warns_in(), None, "a key that never expires");
        let expiring_in = |days: u64| InstanceKey {
            expires: Some(now() + days * DAY_SECS),
            ..InstanceKey::generate(None).unwrap()
        };
        let later = expiring_in(EXPIRY_WARNING_DAYS + 2);
        assert_eq!(later.expiring(), None);
        let wait = later.warns_in().unwrap().as_secs();
        assert!((DAY_SECS..=2 * DAY_SECS + 1).contains(&wait), "{wait}");
        let soon = expiring_in(EXPIRY_WARNING_DAYS - 1);
        assert!(soon.expiring().is_some());
        assert_eq!(soon.warns_in(), None);
    }

    /// A key whose primary key has expired is refused, though its signing
    /// subkey never expires: GnuPG, and pacman through it, takes every
    /// signature under an expired primary key for an expired one.
    #[test]
    fn a_key_whose_primary_key_expired_is_refused() {
        with_gnupg(|tmp, gpg| {
            // Made in 2019, its primary key for a year.
            let made = gpg.unattended(&[
                "--faked-system-time",
                "20190101T000000",
                "--quick-gen-key",
                "Expired <ops@localhost>",
                "ed25519",
                "cert",
                "1y",
            ]);
            let Some(primary) = made.created() else {
                eprintln!("skipped: gpg could not make a key ({})", made.stderr);
                return;
            };
            let added = gpg.unattended(&[
                "--faked-system-time",
                "20190102T000000",
                "--quick-add-key",
                &primary,
                "ed25519",
                "sign",
                "never",
            ]);
            assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
            let exported = gpg.export("--export-secret-keys", &primary, "");
            let parsed = parse_secret(&exported).unwrap();
            assert_eq!(
                subkey_signs(&parsed.secret_subkeys[0], now()),
                Some(None),
                "by itself, the subkey signs for good"
            );
            let path = tmp.join("expired.gpg");
            std::fs::write(&path, &exported).unwrap();

            let error = format!("{:#}", InstanceKey::load(&path, None).unwrap_err());
            assert!(
                error.contains(&format!("primary key {primary} expired on 2020-01-01")),
                "{error}"
            );
        });
    }

    /// A key whose primary key is revoked is refused, though its signing
    /// subkey is not revoked itself.
    #[test]
    fn a_key_whose_primary_key_is_revoked_is_refused() {
        with_gnupg(|tmp, gpg| {
            let Some(primary) = gpg.make("Revoked <ops@localhost>", "cert", "") else {
                return;
            };
            let added = gpg.unattended(&["--quick-add-key", &primary, "ed25519", "sign", "never"]);
            assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
            // The revocation certificate GnuPG made with the key, without the
            // colon that keeps it from being imported by accident.
            let made = std::fs::read_to_string(
                gpg.home
                    .join("openpgp-revocs.d")
                    .join(format!("{primary}.rev")),
            )
            .unwrap();
            let certificate = tmp.join("revocation.asc");
            std::fs::write(
                &certificate,
                made.replace(":-----BEGIN PGP", "-----BEGIN PGP"),
            )
            .unwrap();
            let imported = gpg.unattended(&["--import", certificate.to_str().unwrap()]);
            assert!(imported.success, "gpg --import: {}", imported.stderr);
            let exported = gpg.export("--export-secret-keys", &primary, "");
            let parsed = parse_secret(&exported).unwrap();
            assert!(
                subkey_signs(&parsed.secret_subkeys[0], now()).is_some(),
                "by itself, the subkey signs"
            );
            let path = tmp.join("revoked.gpg");
            std::fs::write(&path, &exported).unwrap();

            let error = format!("{:#}", InstanceKey::load(&path, None).unwrap_err());
            assert!(
                error.contains(&format!("primary key {primary} is revoked")),
                "{error}"
            );
        });
    }

    /// A subkey that never expires under a primary key that does stops
    /// signing when the primary key expires -- as GnuPG says of what it
    /// signed -- so that is when the key is taken to stop signing, and what
    /// the warning within [`EXPIRY_WARNING_DAYS`] names.
    #[test]
    fn a_primary_key_expiring_first_ends_its_subkeys_signing() {
        with_gnupg(|tmp, gpg| {
            for (lifetime, days) in [("10d", 10), ("60d", 60)] {
                // Before the key is made, so a slow machine cannot make it look
                // shorter-lived than it is.
                let before = now();
                let made = gpg.unattended(&[
                    "--quick-gen-key",
                    &format!("Short-lived {days} <ops@localhost>"),
                    "ed25519",
                    "cert",
                    lifetime,
                ]);
                let Some(primary) = made.created() else {
                    eprintln!("skipped: gpg could not make a key ({})", made.stderr);
                    return;
                };
                let added =
                    gpg.unattended(&["--quick-add-key", &primary, "ed25519", "sign", "never"]);
                assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
                let path = tmp.join(format!("short-lived-{days}.gpg"));
                std::fs::write(&path, gpg.export("--export-secret-keys", &primary, "")).unwrap();
                let key = InstanceKey::load(&path, None).unwrap();
                if days < EXPIRY_WARNING_DAYS {
                    let expiring = key.expiring().expect("an expiry within the warning");
                    let warning = expiring.to_string();
                    assert!(
                        warning.contains(&format!("when its primary key {primary} expires")),
                        "{warning}"
                    );
                    assert!(
                        warning.contains(&format!("extend primary key {primary}")),
                        "{warning}"
                    );
                } else {
                    assert_eq!(key.expiring(), None);
                }
                assert_eq!(key.signer, Signer::Subkey(0));
                let expires = key.expires.unwrap();
                assert!(
                    (before + days * DAY_SECS..=now() + days * DAY_SECS).contains(&expires),
                    "{expires}"
                );

                // GnuPG accepts the subkey's signature now, and takes it for an
                // expired one once the primary key has expired.
                let content = tmp.join("content");
                std::fs::write(&content, CONTENT).unwrap();
                let signature = tmp.join("content.sig");
                std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
                let verify = [signature.to_str().unwrap(), content.to_str().unwrap()];
                let verified = gpg.verify(verify);
                assert!(
                    verified.status.contains("[GNUPG:] GOODSIG"),
                    "{}",
                    verified.status
                );
                let later = gpg.run(&[
                    "--faked-system-time",
                    &(expires + 60).to_string(),
                    "--verify",
                    verify[0],
                    verify[1],
                ]);
                assert!(
                    later.status.contains("[GNUPG:] EXPKEYSIG"),
                    "{}",
                    later.status
                );
            }
        });
    }

    /// Once the key that signs has stopped signing -- a server that ran
    /// past it -- it makes no more signatures, which clients would refuse.
    #[test]
    fn a_key_past_its_expiry_does_not_sign() {
        let mut key = InstanceKey::generate(None).unwrap();
        key.expires = Some(now() - 1);
        let error = format!("{:#}", key.sign(CONTENT).unwrap_err());
        assert!(error.contains("stopped signing on"), "{error}");
    }

    /// A supplied key someone else certified -- a colleague's signature on
    /// its user ID, which `gpg --export-secret-keys` includes -- loads: only
    /// its own signatures are checked, and it signs what GnuPG verifies.
    #[test]
    fn a_key_someone_else_certified_loads() {
        with_gnupg(|tmp, gpg| {
            let Some(ours) = gpg.make("Certified <ops@localhost>", "cert", "") else {
                return;
            };
            let added = gpg.unattended(&["--quick-add-key", &ours, "ed25519", "sign", "never"]);
            assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
            let Some(colleague) = gpg.make("Colleague <colleague@localhost>", "cert", "") else {
                return;
            };
            let certified = gpg.unattended(&[
                "--yes",
                "--default-key",
                &colleague,
                "--quick-sign-key",
                &ours,
            ]);
            assert!(
                certified.success,
                "gpg --quick-sign-key: {}",
                certified.stderr
            );
            let exported = gpg.export("--export-secret-keys", &ours, "");
            let parsed = parse_secret(&exported).unwrap();
            assert!(
                parsed
                    .details
                    .users
                    .iter()
                    .flat_map(|user| &user.signatures)
                    .any(|sig| sig
                        .issuer_fingerprint()
                        .iter()
                        .any(|issuer| format!("{issuer:X}") == colleague)),
                "the export carries the colleague's certification"
            );
            assert!(
                parsed.verify_bindings().is_err(),
                "rPGP checks a certification it has no key for again; `self_signed` may go"
            );
            let path = tmp.join("certified.gpg");
            std::fs::write(&path, &exported).unwrap();

            let key = InstanceKey::load(&path, None).unwrap();
            assert_eq!(key.fingerprint(), ours);
            let content = tmp.join("content");
            std::fs::write(&content, CONTENT).unwrap();
            let signature = tmp.join("content.sig");
            std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
            let verified = gpg.verify([signature.to_str().unwrap(), content.to_str().unwrap()]);
            assert!(verified.success, "gpg --verify: {}", verified.stderr);
            assert!(key.verifies(&std::fs::read(&signature).unwrap(), CONTENT));
            assert!(!key.verifies(&std::fs::read(&signature).unwrap(), b"something else"));
        });
    }

    /// About one unprotected Ed25519 secret GnuPG exports in 256 -- one
    /// beginning with a zero byte, written with that leading zero octet,
    /// which the standard forbids -- is one rPGP refuses ("Invalid
    /// checksum"), though GnuPG reads it. It is refused saying how to get a
    /// copy rPGP reads, and that way, locked with a passphrase, it loads and
    /// signs.
    #[test]
    fn a_key_gnupg_wrote_off_the_standard_is_refused_with_a_way_around() {
        let tmp = tempfile::tempdir().unwrap();
        let off_standard = include_bytes!("../testdata/gnupg-unprotected-leading-zero.asc");
        let path = tmp.path().join("off-standard.asc");
        std::fs::write(&path, off_standard).unwrap();
        let error = format!("{:#}", InstanceKey::load(&path, None).unwrap_err());
        assert!(
            error.contains(REFUSED_CHECKSUM),
            "rPGP reads such keys now, and the refusal can go: {error}"
        );

        let Some(gpg) = Gpg::new(tmp.path()) else {
            eprintln!("skipped the way around: gpg is not installed");
            return;
        };
        let imported = gpg.unattended(&["--import", path.to_str().unwrap()]);
        assert!(imported.success, "gpg --import: {}", imported.stderr);
        let fingerprint = imported
            .status
            .lines()
            .find_map(|line| line.strip_prefix("[GNUPG:] IMPORT_OK "))
            .and_then(|rest| rest.split_whitespace().nth(1))
            .expect("gpg names the key it imported")
            .to_string();
        // With no passphrase yet, `--passwd` asks only for the new one.
        let locked = gpg.with_passphrase("correct horse", &["--passwd", &fingerprint]);
        assert!(locked.success, "gpg --passwd: {}", locked.stderr);
        let path = tmp.path().join("locked.gpg");
        std::fs::write(
            &path,
            gpg.export("--export-secret-keys", &fingerprint, "correct horse"),
        )
        .unwrap();
        let key = InstanceKey::load(&path, Some("correct horse")).unwrap();
        assert_eq!(key.fingerprint(), fingerprint);
        assert!(key.verifies(&key.sign(CONTENT).unwrap(), CONTENT));
    }

    /// A key kept in the repository directory, as the hybrid image's CA in
    /// `repo/.ca` is, is told apart from one kept beside it, a symlink to
    /// it included.
    #[test]
    fn a_key_inside_the_repository_is_told_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".ca")).unwrap();
        std::fs::create_dir_all(tmp.path().join("data/ca")).unwrap();
        let inside = repo.join(".ca").join(KEY_FILE);
        let outside = tmp.path().join("data/ca").join(KEY_FILE);
        std::fs::write(&inside, "").unwrap();
        std::fs::write(&outside, "").unwrap();
        assert!(is_inside(&inside, &repo));
        assert!(!is_inside(&outside, &repo));
        #[cfg(unix)]
        {
            let link = tmp.path().join("linked.asc");
            std::os::unix::fs::symlink(&inside, &link).unwrap();
            assert!(is_inside(&link, &repo));
        }
    }

    /// Run `test` with a throwaway GnuPG home in a temporary directory, or
    /// skip it when GnuPG is not installed. A run that fails because GnuPG
    /// wrote a key off the standard, which rPGP refuses
    /// ([`REFUSED_CHECKSUM`]) -- about one unprotected secret in 256, which no
    /// test calling this is about -- is run again, with new keys.
    fn with_gnupg(test: impl Fn(&Path, &Gpg)) {
        for _ in 0..5 {
            let tmp = tempfile::tempdir().unwrap();
            let Some(gpg) = Gpg::new(tmp.path()) else {
                eprintln!("skipped: gpg is not installed");
                return;
            };
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                test(tmp.path(), &gpg);
            }));
            let Err(panic) = run else {
                return;
            };
            let said = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or_default();
            if !said.contains(REFUSED_CHECKSUM) {
                std::panic::resume_unwind(panic);
            }
            eprintln!("GnuPG made a key rPGP refuses; running again with new keys");
        }
        panic!("GnuPG made keys rPGP refuses five times running");
    }

    /// A throwaway GnuPG home, whose agent is stopped when it is dropped.
    struct Gpg {
        home: std::path::PathBuf,
    }

    impl Gpg {
        /// A home in `dir`, or `None` when GnuPG is not installed.
        fn new(dir: &Path) -> Option<Self> {
            Command::new("gpg").arg("--version").output().ok()?;
            let home = dir.join("gnupg");
            std::fs::create_dir(&home).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            Some(Self { home })
        }

        fn run(&self, args: &[&str]) -> Output {
            let output = Command::new("gpg")
                .arg("--homedir")
                .arg(&self.home)
                .args(["--batch", "--no-tty", "--status-fd", "1"])
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            Output {
                success: output.status.success(),
                status: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }

        /// A run that would otherwise ask for a passphrase, given none.
        fn unattended(&self, args: &[&str]) -> Output {
            self.with_passphrase("", args)
        }

        /// A run given `passphrase` wherever it would ask for one.
        fn with_passphrase(&self, passphrase: &str, args: &[&str]) -> Output {
            let mut all = vec!["--pinentry-mode", "loopback", "--passphrase", passphrase];
            all.extend_from_slice(args);
            self.run(&all)
        }

        /// A new ed25519 primary key for `user`, with `usage`, locked with
        /// `passphrase` unless it is empty: its fingerprint, or `None` (the
        /// test skipped) when this GnuPG cannot make one.
        fn make(&self, user: &str, usage: &str, passphrase: &str) -> Option<String> {
            let made = self.with_passphrase(
                passphrase,
                &["--quick-gen-key", user, "ed25519", usage, "never"],
            );
            let primary = made.created();
            if primary.is_none() {
                eprintln!("skipped: gpg could not make a key ({})", made.stderr);
            }
            primary
        }

        /// What `gpg <how> <key>` exports, binary.
        fn export(&self, how: &str, key: &str, passphrase: &str) -> Vec<u8> {
            let exported = Command::new("gpg")
                .arg("--homedir")
                .arg(&self.home)
                .args(["--batch", "--pinentry-mode", "loopback", "--passphrase"])
                .arg(passphrase)
                .args([how, key])
                .output()
                .unwrap();
            assert!(
                exported.status.success(),
                "gpg {how}: {}",
                String::from_utf8_lossy(&exported.stderr)
            );
            exported.stdout
        }

        fn verify(&self, [signature, content]: [&str; 2]) -> Output {
            self.run(&["--verify", signature, content])
        }
    }

    impl Drop for Gpg {
        fn drop(&mut self) {
            let _ = Command::new("gpgconf")
                .arg("--homedir")
                .arg(&self.home)
                .args(["--kill", "all"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    /// What one `gpg` run said.
    struct Output {
        success: bool,
        status: String,
        stderr: String,
    }

    impl Output {
        /// The fingerprint of the primary key a successful `--quick-gen-key`
        /// made, or `None` when it made none.
        fn created(&self) -> Option<String> {
            if !self.success {
                return None;
            }
            let primary = self
                .status
                .lines()
                .find_map(|line| line.strip_prefix("[GNUPG:] KEY_CREATED P "))
                .expect("gpg names the key it made");
            Some(primary.trim().to_string())
        }

        /// The fingerprint of the subkey a successful `--quick-add-key`
        /// made, or `None` when it made none.
        fn created_subkey(&self) -> Option<String> {
            if !self.success {
                return None;
            }
            self.status
                .lines()
                .find_map(|line| line.strip_prefix("[GNUPG:] KEY_CREATED S "))
                .map(|subkey| subkey.trim().to_string())
        }
    }
}
