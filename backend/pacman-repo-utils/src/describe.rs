use anyhow::{anyhow, bail};

use crate::pkginfo::parser::Pkginfo;
use crate::repo_database::desc::Desc;
use liblzma::bufread::XzDecoder;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::Path;
use tar::Archive;
use tracing::{debug, error};
use zstd::stream::read::Decoder as ZstdDecoder;

/// What one package contributes to a repository's databases.
///
/// Produced by [`describe_package`] from the package file alone, so the slow
/// and failure-prone part of adding a package -- reading the whole archive --
/// happens before, and apart from, any change to the databases themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    /// The package file, as it is named in the repository.
    pub filename: String,
    /// `{pkgname}-{pkgver}`: the directory this entry occupies in both
    /// databases.
    pub dir_name: String,
    /// The `desc` file both databases carry.
    pub(crate) desc: String,
    /// The `files` file only `repo.files` carries.
    pub(crate) files: String,
}

/// Read a package file into the entry it gets in `repo.db` and `repo.files`:
/// its `.PKGINFO`, the files it installs, its size and its checksums, plus a
/// detached `<file>.sig` beside it if there is one.
///
/// Reads the whole archive, streamed: memory stays flat however large the
/// package is.
pub fn describe_package(pkgfile: &Path) -> anyhow::Result<PackageEntry> {
    let mut files = vec![];
    let mut pkginfo = Pkginfo::new();

    let file = File::open(pkgfile)?;
    let ext = pkgfile.extension().and_then(|e| e.to_str());
    let decompressor: Box<dyn Read> = match ext {
        Some("zst") => Box::new(ZstdDecoder::new(BufReader::new(file))?),
        Some("xz") => Box::new(XzDecoder::new(BufReader::new(file))),
        _ => bail!("Unsupported file type"),
    };
    let mut archive = Archive::new(decompressor);

    let pkgpath = pkgfile.display();
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let name = path.display().to_string();
        if !name.starts_with('.') {
            files.push(name);
        }

        if path == Path::new(".PKGINFO") {
            debug!("Found .PKGINFO file in '{pkgpath}'.");
            pkginfo.parse(entry)?;
        }
    }

    if !pkginfo.valid() {
        error!("Invalid package file '{pkgpath}'.");
        bail!("Invalid package file");
    }

    pkginfo.set_signature(pkgfile)?;

    let csize = fs::metadata(pkgfile)?.len();
    let Checksums { md5sum, sha256sum } = calc_checksums(pkgfile)?;

    let filename = pkgfile
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("invalid package path: {pkgpath}"))?
        .to_string();

    let dir_name = format!("{}-{}", pkginfo.pkgname, pkginfo.pkgver);

    let mut desc = Desc::from(pkginfo);
    desc.filename.clone_from(&filename);
    desc.md5sum = md5sum;
    desc.csize = csize.to_string();
    desc.sha256sum = sha256sum;

    files.sort_unstable();
    Ok(PackageEntry {
        filename,
        dir_name,
        desc: desc.to_string(),
        files: format!("%FILES%\n{}", files.join("\n")),
    })
}

/// A file's digests, lowercase hex, named after the `desc` fields they fill.
struct Checksums {
    md5sum: String,
    sha256sum: String,
}

/// md5 and sha256 of a file, in one streamed pass.
fn calc_checksums(path: &Path) -> anyhow::Result<Checksums> {
    let mut file = File::open(path)?;
    let mut md5 = md5::Context::new();
    let mut sha256 = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        md5.consume(&buffer[..read]);
        sha256.update(&buffer[..read]);
    }
    let sha256sum = sha256
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(Checksums {
        md5sum: format!("{:x}", md5.finalize()),
        sha256sum,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_corrupt_member_after_valid_metadata_is_rejected() {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let content = b"pkgname = demo\npkgver = 1.0-1\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, ".PKGINFO", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }
        // Replace the end marker with an invalid tar header after .PKGINFO.
        bytes.truncate(bytes.len() - 1024);
        bytes.extend_from_slice(&[b'x'; 512]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("demo-1.0-1-x86_64.pkg.tar.zst");
        fs::write(&path, zstd::stream::encode_all(&bytes[..], 0).unwrap()).unwrap();

        assert!(describe_package(&path).is_err());
    }

    /// The streamed checksums are the checksums of the whole file, across
    /// buffer boundaries.
    #[test]
    fn checksums_span_the_whole_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big");
        let bytes: Vec<u8> = (0..(3 << 20) + 17).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &bytes).unwrap();

        let Checksums { md5sum, sha256sum } = calc_checksums(&path).unwrap();

        assert_eq!(md5sum, format!("{:x}", md5::compute(&bytes)));
        assert_eq!(
            sha256sum,
            Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
    }
}
