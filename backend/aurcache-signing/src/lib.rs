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
use pgp::packet::SignatureType;
use pgp::ser::Serialize;
use pgp::types::{Fingerprint, KeyDetails, KeyId, KeyVersion, Password, Timestamp};
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

/// The instance key, unlocked and ready to sign.
pub struct InstanceKey {
    key: SignedSecretKey,
    signer: Signer,
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
        let key = SignedSecretKey::from_armor_single(bytes.as_slice())
            .map(|(key, _headers)| key)
            .or_else(|_| SignedSecretKey::from_bytes(bytes.as_slice()))
            .with_context(|| format!("{} is not an OpenPGP secret key", path.display()))?;
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
        let signer = signer(&key);
        let fingerprint = format!("{:X}", key.primary_key.fingerprint());
        let public_key = SignedPublicKey::from(key.clone())
            .to_armored_string(ArmorOptions::default())
            .context("armoring the public key")?;
        let key = Self {
            key,
            signer,
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

    /// Whether this key made `signature`, as the issuer it names. Says nothing
    /// about what it signs: see [`Self::verifies`].
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

/// How a signature names the key that made it.
struct Issuer {
    fingerprint: Fingerprint,
    id: KeyId,
}

/// The newest subkey that signs now, else the primary key, as GnuPG picks: a
/// key this server generated signs with its one subkey, and one an operator
/// supplies -- whose old subkeys may have expired -- with its newest valid one.
fn signer(key: &SignedSecretKey) -> Signer {
    let now = u64::from(Timestamp::now().as_secs());
    key.secret_subkeys
        .iter()
        .enumerate()
        .filter(|(_, subkey)| signs_at(subkey, now))
        .max_by_key(|(_, subkey)| subkey.key.created_at().as_secs())
        .map_or(Signer::Primary, |(index, _)| Signer::Subkey(index))
}

/// Whether `subkey` signs at `now` (seconds): its latest binding says it
/// signs and has not expired, and nothing revokes it.
fn signs_at(subkey: &SignedSecretSubKey, now: u64) -> bool {
    let revoked = subkey
        .signatures
        .iter()
        .any(|sig| sig.typ() == Some(SignatureType::SubkeyRevocation));
    let binding = subkey
        .signatures
        .iter()
        .filter(|sig| sig.typ() == Some(SignatureType::SubkeyBinding))
        .max_by_key(|sig| sig.created().map_or(0, Timestamp::as_secs));
    let Some(binding) = binding else {
        return false;
    };
    let created = u64::from(subkey.key.created_at().as_secs());
    let expired = binding
        .key_expiration_time()
        .map(|lifetime| u64::from(lifetime.as_secs()))
        .is_some_and(|lifetime| lifetime > 0 && created + lifetime <= now);
    binding.key_flags().sign() && !revoked && !expired
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
        assert_eq!(key.signer, Signer::Subkey(1));
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
            let mut all = vec!["--pinentry-mode", "loopback", "--passphrase", ""];
            all.extend_from_slice(args);
            self.run(&all)
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
