//! The repository's signing key.
//!
//! One OpenPGP key per instance signs every package the repository publishes
//! and both of its databases, so a client trusts one key, once. It is built to
//! be a root: an Ed25519 primary key that only certifies, and an Ed25519
//! subkey under it that signs. Clients trust the primary key, so another
//! signing key certified under it later is trusted without asking them again.
//!
//! The key is a file, like the worker CA's, never a database row: the
//! database is what gets exported, backed up elsewhere and inspected. Nothing
//! here hands the secret key out, and its `Debug` shows the fingerprint only.

use anyhow::{Context, anyhow, bail};
use aurcache_common::server_env;
use aurcache_common::settings::parse_boolean;
use pgp::composed::{
    ArmorOptions, Deserializable, DetachedSignature, KeyType, SecretKeyParamsBuilder,
    SignedPublicKey, SignedSecretKey, SignedSecretSubKey, SubkeyParamsBuilder,
};
use pgp::crypto::hash::HashAlgorithm;
use pgp::packet::{Signature, SignatureType, SubpacketData};
use pgp::ser::Serialize;
use pgp::types::{
    Fingerprint, KeyDetails, KeyId, KeyVersion, Password, S2kParams, SecretParams, StringToKey,
    Timestamp,
};
use std::io::{BufReader, Read};
use std::path::Path;

/// The key's file name in the server's CA directory, where it is kept unless
/// `AURCACHE_SIGNING_KEY_FILE` names another.
pub const KEY_FILE: &str = "signing-key.asc";

/// The key's user ID. Clients identify the key by its fingerprint; this is
/// what `pacman-key --list-keys` shows beside it.
const USER_ID: &str = "AURCache repository";

/// What signatures hash with: what GnuPG itself uses with an Ed25519 key.
const HASH: HashAlgorithm = HashAlgorithm::Sha512;

/// How long before the key that signs expires the server starts saying so.
const EXPIRY_NOTICE_SECS: u64 = 30 * 24 * 60 * 60;

/// The instance key, unlocked and ready to sign.
///
/// Which of its component keys signs is chosen again for each signature
/// ([`choose`]), so a subkey that expires while the server runs gives way to
/// the next valid one rather than go on making signatures clients refuse.
pub struct InstanceKey {
    key: SignedSecretKey,
    passphrase: Password,
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
        return Ok(None);
    }
    let passphrase = server_env::AURCACHE_SIGNING_KEY_PASSPHRASE_FILE
        .get_os()
        .map(|path| read_passphrase(Path::new(&path)))
        .transpose()?;
    let key = match server_env::AURCACHE_SIGNING_KEY_FILE.get_os() {
        Some(path) => InstanceKey::load(Path::new(&path), passphrase.as_deref()),
        None => InstanceKey::load_or_create(&ca_dir.join(KEY_FILE), passphrase.as_deref()),
    }?;
    Ok(Some(key))
}

/// A secret key, armored or binary.
fn parse_secret(bytes: &[u8]) -> anyhow::Result<SignedSecretKey> {
    let armored = match SignedSecretKey::from_armor_single(bytes) {
        Ok((key, _headers)) => return Ok(key),
        Err(error) => error,
    };
    let binary = match SignedSecretKey::from_bytes(bytes) {
        Ok(key) => return Ok(key),
        Err(error) => error,
    };
    if [armored, binary]
        .iter()
        .any(|error| error.to_string().contains("Invalid checksum"))
    {
        bail!(UNREADABLE_UNPROTECTED);
    }
    bail!("not an OpenPGP secret key")
}

/// Why a key GnuPG reads can be refused here, and the way around it.
///
/// rPGP (0.21) checks an unprotected secret key's checksum over its own
/// encoding of the secret, not over the bytes in the file, and GnuPG writes
/// an Ed25519 secret whose first byte is zero with a bit count rPGP does not
/// keep -- about one secret in 256. A passphrase moves the check to the
/// decrypted bytes, which rPGP gets right.
const UNREADABLE_UNPROTECTED: &str = "its checksum does not match as rPGP, which reads \
     signing keys here, computes it: GnuPG writes about one unprotected Ed25519 secret in \
     256 that way. Lock it with a passphrase (gpg --passwd), export it again, and give the \
     passphrase with AURCACHE_SIGNING_KEY_PASSPHRASE_FILE";

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
    /// Refused unless it signs: a key that does not unlock fails here rather
    /// than at the first publish.
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
        key.verify_bindings()
            .context("the key's self-signatures do not verify")?;
        let now = now();
        let choice = choose(&key, now).ok_or_else(|| anyhow!(NOTHING_SIGNS))?;
        if let Some(notice) = expiry_notice(&key, choice, now) {
            tracing::warn!("{notice}");
        }
        let fingerprint = format!("{:X}", key.primary_key.fingerprint());
        let public_key = SignedPublicKey::from(key.clone())
            .to_armored_string(ArmorOptions::default())
            .context("armoring the public key")?;
        let key = Self {
            key,
            passphrase,
            fingerprint,
            public_key,
        };
        let probe = key
            .sign(&b""[..])
            .context("the key does not sign; is its passphrase right?")?;
        if !key.verifies(&probe, b"") {
            bail!("a signature the key made does not verify");
        }
        Ok(key)
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
    /// `.sig` file or from `%PGPSIG%`.
    pub fn sign(&self, data: impl Read) -> anyhow::Result<Vec<u8>> {
        let rng = rand::thread_rng();
        let data = BufReader::new(data);
        let signature = match self.signer()? {
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

    /// Whether this key made `signature`, as the issuer it names. Says nothing
    /// about what it signs: see [`Self::verifies`].
    #[must_use]
    pub fn issued(&self, signature: &[u8]) -> bool {
        let Ok(parsed) = DetachedSignature::from_bytes(signature) else {
            return false;
        };
        let Ok(Issuer { fingerprint, id }) = self.issuer() else {
            return false;
        };
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
        let Ok(signer) = self.signer() else {
            return false;
        };
        match signer {
            Signer::Primary => parsed
                .verify(self.key.primary_key.public_key(), content)
                .is_ok(),
            Signer::Subkey(index) => parsed
                .verify(self.key.secret_subkeys[index].key.public_key(), content)
                .is_ok(),
        }
    }

    fn issuer(&self) -> anyhow::Result<Issuer> {
        Ok(match self.signer()? {
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
        })
    }

    /// The component key that signs now.
    fn signer(&self) -> anyhow::Result<Signer> {
        choose(&self.key, now())
            .map(|choice| choice.signer)
            .ok_or_else(|| anyhow!(NOTHING_SIGNS))
    }
}

/// Why a key is refused, or stops signing, when no component key of it signs.
const NOTHING_SIGNS: &str = "nothing in the signing key signs now: no subkey that signs \
     is current (neither expired nor revoked) with its secret in the file, and the primary \
     key does not sign either";

/// The current time, in seconds since the epoch, as key times are kept.
fn now() -> u64 {
    u64::from(Timestamp::now().as_secs())
}

/// How a signature names the key that made it.
struct Issuer {
    fingerprint: Fingerprint,
    id: KeyId,
}

/// The component key that signs at a time, and until when it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Choice {
    signer: Signer,
    /// When it expires, in seconds since the epoch; `None` for never.
    expires: Option<u64>,
}

/// How long a component key that signs goes on signing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Validity {
    /// When it expires, in seconds since the epoch; `None` for never.
    expires: Option<u64>,
}

/// The newest subkey that signs at `now`, else the primary key if it does, as
/// GnuPG picks: a key this server generated signs with its one subkey, and
/// one an operator supplies -- whose old subkeys may have expired -- with its
/// newest valid one. `None` when nothing in it signs.
fn choose(key: &SignedSecretKey, now: u64) -> Option<Choice> {
    let subkey = key
        .secret_subkeys
        .iter()
        .enumerate()
        .filter_map(|(index, subkey)| {
            subkey_signs(subkey, now).map(|validity| Candidate {
                index,
                created: subkey.key.created_at().as_secs(),
                validity,
            })
        })
        .max_by_key(|candidate| candidate.created);
    match subkey {
        Some(candidate) => Some(Choice {
            signer: Signer::Subkey(candidate.index),
            expires: candidate.validity.expires,
        }),
        None => primary_signs(key, now).map(|validity| Choice {
            signer: Signer::Primary,
            expires: validity.expires,
        }),
    }
}

/// A subkey that signs, while [`choose`] looks for the newest.
struct Candidate {
    index: usize,
    created: u32,
    validity: Validity,
}

/// Whether `subkey` signs at `now` (seconds): its latest binding says it
/// signs and has not expired, nothing revokes it, and its secret is in the
/// file.
fn subkey_signs(subkey: &SignedSecretSubKey, now: u64) -> Option<Validity> {
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
    current(u64::from(subkey.key.created_at().as_secs()), binding, now)
}

/// Whether the primary key signs at `now`, as [`subkey_signs`] says of a
/// subkey: by its newest self-signature, a direct one or a user ID's.
fn primary_signs(key: &SignedSecretKey, now: u64) -> Option<Validity> {
    let primary = &key.primary_key;
    let revoked = key
        .details
        .revocation_signatures
        .iter()
        .any(|sig| sig.typ() == Some(SignatureType::KeyRevocation));
    if revoked || !holds_secret(primary.secret_params()) {
        return None;
    }
    let fingerprint = primary.fingerprint();
    let id = primary.legacy_key_id();
    let newest = key
        .details
        .direct_signatures
        .iter()
        .chain(key.details.users.iter().flat_map(|user| &user.signatures))
        .filter(|sig| {
            sig.issuer_fingerprint().contains(&&fingerprint) || sig.issuer_key_id().contains(&&id)
        })
        .max_by_key(|sig| sig.created().map_or(0, Timestamp::as_secs))?;
    // Without key flags, what the key does is its algorithm's to say; the
    // probe signature at load tells a key that cannot sign.
    if states_key_flags(newest) && !newest.key_flags().sign() {
        return None;
    }
    current(u64::from(primary.created_at().as_secs()), newest, now)
}

/// How long a key created at `created`, with `sig` its self-signature, signs
/// after `now`; `None` once it has expired.
fn current(created: u64, sig: &Signature, now: u64) -> Option<Validity> {
    let expires = sig
        .key_expiration_time()
        .map(|lifetime| u64::from(lifetime.as_secs()))
        .filter(|lifetime| *lifetime > 0)
        .map(|lifetime| created + lifetime);
    match expires {
        Some(at) if at <= now => None,
        _ => Some(Validity { expires }),
    }
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

/// What to tell the operator when the key that signs at `now` expires
/// within [`EXPIRY_NOTICE_SECS`]: when, and what signs after it.
fn expiry_notice(key: &SignedSecretKey, choice: Choice, now: u64) -> Option<String> {
    let expires = choice.expires?;
    if expires > now + EXPIRY_NOTICE_SECS {
        return None;
    }
    let days = (expires - now) / (24 * 60 * 60);
    let then = match choose(key, expires) {
        Some(next) => format!("then its {} signs", describe(key, next.signer)),
        None => "then nothing in it signs, and publishing fails until the server has a key \
                 with a current signing subkey"
            .to_string(),
    };
    Some(format!(
        "the repository's signing {} expires in {days} days; {then}",
        describe(key, choice.signer)
    ))
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
        assert_eq!(key.signer().unwrap(), Signer::Subkey(0));
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
        let tmp = tempfile::tempdir().unwrap();
        let Some(gpg) = Gpg::new(tmp.path()) else {
            eprintln!("skipped: gpg is not installed");
            return;
        };
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
            let path = tmp
                .path()
                .join(format!("supplied-{}.asc", passphrase.len()));
            std::fs::write(&path, &exported.stdout).unwrap();

            let key = InstanceKey::load(&path, Some(passphrase).filter(|p| !p.is_empty())).unwrap();
            let content = tmp.path().join("content");
            std::fs::write(&content, CONTENT).unwrap();
            let signature = tmp.path().join("content.sig");
            std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
            let verified = gpg.verify([signature.to_str().unwrap(), content.to_str().unwrap()]);
            assert!(verified.success, "gpg --verify: {}", verified.stderr);
            assert!(
                verified.status.contains(key.fingerprint()),
                "{}",
                verified.status
            );
        }
    }

    /// A supplied key whose first signing subkey has expired -- as GnuPG
    /// keys do, by default -- signs with its newest valid one, whose
    /// signatures GnuPG accepts.
    #[test]
    fn an_expired_subkey_is_passed_over() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(gpg) = Gpg::new(tmp.path()) else {
            eprintln!("skipped: gpg is not installed");
            return;
        };
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
        let current = gpg.unattended(&["--quick-add-key", &primary, "ed25519", "sign", "never"]);
        assert!(current.success, "gpg --quick-add-key: {}", current.stderr);
        let exported = Command::new("gpg")
            .arg("--homedir")
            .arg(&gpg.home)
            .args(["--batch", "--pinentry-mode", "loopback", "--passphrase", ""])
            .args(["--armor", "--export-secret-keys", &primary])
            .output()
            .unwrap();
        assert!(exported.status.success());
        let path = tmp.path().join("rotated.asc");
        std::fs::write(&path, &exported.stdout).unwrap();

        let key = InstanceKey::load(&path, None).unwrap();
        assert_eq!(key.signer().unwrap(), Signer::Subkey(1));
        let content = tmp.path().join("content");
        std::fs::write(&content, CONTENT).unwrap();
        let signature = tmp.path().join("content.sig");
        std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
        let verified = gpg.verify([signature.to_str().unwrap(), content.to_str().unwrap()]);
        assert!(verified.success, "gpg --verify: {}", verified.stderr);
        assert!(
            verified.status.contains("[GNUPG:] GOODSIG") && !verified.status.contains("EXPKEYSIG"),
            "{}",
            verified.status
        );
    }

    /// A key whose primary key stays offline -- `gpg --export-secret-subkeys`,
    /// which leaves a stub where its secret was -- signs with its subkey,
    /// locked or not; one whose only key that signs is that primary is
    /// refused for what it is, not for a passphrase.
    #[test]
    fn a_primary_key_kept_offline_is_not_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(gpg) = Gpg::new(tmp.path()) else {
            eprintln!("skipped: gpg is not installed");
            return;
        };
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
            let path = tmp.path().join(format!("offline-{}.gpg", passphrase.len()));
            std::fs::write(
                &path,
                gpg.export("--export-secret-subkeys", &primary, passphrase),
            )
            .unwrap();

            let key = InstanceKey::load(&path, Some(passphrase).filter(|p| !p.is_empty())).unwrap();
            assert!(!holds_secret(key.key.primary_key.secret_params()));
            assert_eq!(key.signer().unwrap(), Signer::Subkey(0));
            let content = tmp.path().join("content");
            std::fs::write(&content, CONTENT).unwrap();
            let signature = tmp.path().join("content.sig");
            std::fs::write(&signature, key.sign_file(&content).unwrap()).unwrap();
            let verified = gpg.verify([signature.to_str().unwrap(), content.to_str().unwrap()]);
            assert!(verified.success, "gpg --verify: {}", verified.stderr);
        }

        let Some(primary) = gpg.make("Signs offline <ops@localhost>", "sign", "") else {
            return;
        };
        let added = gpg.unattended(&["--quick-add-key", &primary, "cv25519", "encr", "never"]);
        assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
        let path = tmp.path().join("signs-offline.gpg");
        std::fs::write(&path, gpg.export("--export-secret-subkeys", &primary, "")).unwrap();
        let error = format!("{:#}", InstanceKey::load(&path, None).unwrap_err());
        assert!(error.contains(NOTHING_SIGNS), "{error}");
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

    /// A signing subkey close to expiring is announced, with what signs once
    /// it has; at that time the next one signs.
    #[test]
    fn a_subkey_close_to_expiring_is_announced() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(gpg) = Gpg::new(tmp.path()) else {
            eprintln!("skipped: gpg is not installed");
            return;
        };
        let Some(primary) = gpg.make("Expiring <ops@localhost>", "cert", "") else {
            return;
        };
        for lifetime in ["1y", "10d"] {
            let added = gpg.unattended(&["--quick-add-key", &primary, "ed25519", "sign", lifetime]);
            assert!(added.success, "gpg --quick-add-key: {}", added.stderr);
            // Subkeys are told apart by their creation second.
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        let path = tmp.path().join("expiring.gpg");
        std::fs::write(&path, gpg.export("--export-secret-keys", &primary, "")).unwrap();
        let key = InstanceKey::load(&path, None).unwrap();

        let now = now();
        let choice = choose(&key.key, now).unwrap();
        assert_eq!(choice.signer, Signer::Subkey(1));
        let notice = expiry_notice(&key.key, choice, now).unwrap();
        let next = describe(&key.key, Signer::Subkey(0));
        assert!(notice.contains("expires in 9 days"), "{notice}");
        assert!(
            notice.contains(&format!("then its {next} signs")),
            "{notice}"
        );

        let after = choose(&key.key, choice.expires.unwrap()).unwrap();
        assert_eq!(after.signer, Signer::Subkey(0));
        assert_eq!(expiry_notice(&key.key, after, now), None);
    }

    /// About one unprotected Ed25519 key GnuPG exports in 256 -- a secret
    /// beginning with a zero byte -- is one rPGP refuses ("Invalid
    /// checksum"), though GnuPG reads it: refused here with the way around it,
    /// which this same key, locked with `gpg --passwd`, shows works. When this
    /// test fails because the key loads, rPGP reads such keys, and the advice
    /// can go.
    #[test]
    fn a_gnupg_key_rpgp_cannot_read_is_refused_with_the_way_around_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("unprotected.gpg");
        std::fs::write(
            &path,
            include_bytes!("../testdata/gnupg-unprotected-leading-zero.gpg"),
        )
        .unwrap();
        let error = format!("{:#}", InstanceKey::load(&path, None).unwrap_err());
        assert!(error.contains(UNREADABLE_UNPROTECTED), "{error}");

        let path = tmp.path().join("protected.gpg");
        std::fs::write(
            &path,
            include_bytes!("../testdata/gnupg-protected-leading-zero.gpg"),
        )
        .unwrap();
        let key = InstanceKey::load(&path, Some("testpw")).unwrap();
        assert!(key.verifies(&key.sign(CONTENT).unwrap(), CONTENT));
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
            if !made.success {
                eprintln!("skipped: gpg could not make a key ({})", made.stderr);
                return None;
            }
            let primary = made
                .status
                .lines()
                .find_map(|line| line.strip_prefix("[GNUPG:] KEY_CREATED P "))
                .expect("gpg names the key it made");
            Some(primary.trim().to_string())
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
}
