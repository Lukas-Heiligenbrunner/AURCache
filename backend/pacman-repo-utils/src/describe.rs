use anyhow::{anyhow, bail};

use crate::pkginfo::parser::Pkginfo;
use crate::repo_database::desc::Desc;
use liblzma::bufread::XzDecoder;
use liblzma::stream::Stream;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::Path;
use tar::Archive;
use tracing::error;
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
    /// The file's SHA-256, lowercase hex, as `%SHA256SUM%` records it.
    pub sha256sum: String,
    /// The `desc` file both databases carry.
    pub(crate) desc: String,
    /// The `files` file only `repo.files` carries.
    pub(crate) files: String,
}

// Bounds on reading one package archive, which whoever built the package
// controls: a few KiB of zstd or xz can expand to terabytes. They bound one
// read, to about 650 MB, not the server.

/// The largest tar extension header (GNU long name or link, pax record) read.
/// A real one holds a path.
const MAX_EXTENSION_BYTES: u64 = 64 * 1024;

/// The most memory xz's decoder may take: a stream otherwise picks its own
/// dictionary, up to 1.5 GiB. `xz -9` needs 64 MiB.
const MAX_XZ_MEMORY: u64 = 256 << 20;

/// Paths the file list may hold. Each costs an allocation beyond its bytes, so
/// without this one-letter paths could take gigabytes. Unreal Engine, the
/// largest package seen, has about 540,000.
const MAX_PATHS: usize = 4_000_000;

/// Bytes of the file list, as `repo.files` holds it. Unreal Engine's is about
/// 60 MB.
const MAX_LISTING_BYTES: usize = 256 << 20;

/// How many times its file's size a package may decompress to. This is what
/// bounds a read's time. Real packages stay under 100.
const MAX_EXPANSION: u64 = 1024;

/// The least a package may decompress to: a ratio says little about a small
/// file.
const MIN_DECOMPRESSED_LIMIT: u64 = 64 << 20;

fn decompressed_limit(csize: u64) -> u64 {
    csize
        .saturating_mul(MAX_EXPANSION)
        .max(MIN_DECOMPRESSED_LIMIT)
}

fn decompress(pkgfile: &Path) -> anyhow::Result<Box<dyn Read>> {
    let file = File::open(pkgfile)?;
    let ext = pkgfile.extension().and_then(|e| e.to_str());
    Ok(match ext {
        Some("zst") => Box::new(ZstdDecoder::new(file)?),
        Some("xz") => Box::new(XzDecoder::new_stream(
            BufReader::new(file),
            Stream::new_stream_decoder(MAX_XZ_MEMORY, 0)?,
        )),
        _ => bail!("Unsupported file type"),
    })
}

/// What the extension headers since the last member say about the next one.
#[derive(Default)]
struct Pending {
    long_name: Option<String>,
    pax_path: Option<String>,
}

impl Pending {
    /// GNU long links and pax global headers name no path, and are ignored.
    fn add(&mut self, kind: tar::EntryType, data: &[u8]) -> anyhow::Result<()> {
        if kind.is_gnu_longname() {
            let name = data.split(|b| *b == 0).next().unwrap_or_default();
            self.long_name = Some(String::from_utf8_lossy(name).into_owned());
        } else if kind.is_pax_local_extensions() {
            for extension in tar::PaxExtensions::new(data) {
                let extension = extension?;
                if extension.key() == Ok("path") {
                    self.pax_path =
                        Some(String::from_utf8_lossy(extension.value_bytes()).into_owned());
                }
            }
        }
        Ok(())
    }

    fn path(self) -> Option<String> {
        self.pax_path.or(self.long_name)
    }
}

fn is_extension(kind: tar::EntryType) -> bool {
    kind.is_gnu_longname()
        || kind.is_gnu_longlink()
        || kind.is_pax_local_extensions()
        || kind.is_pax_global_extensions()
}

/// Read raw, with long names and pax paths resolved here: the tar crate reads
/// each extension header whole, whatever size it claims.
fn walk<R: Read>(
    archive: &mut Archive<R>,
    mut visit: impl FnMut(&str, &mut tar::Entry<'_, R>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut pending = Pending::default();
    // Raw, a member's size is its ustar header's, never a pax `size`: bsdtar
    // always writes the header's too.
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if is_extension(kind) {
            let size = entry.header().entry_size()?;
            if size > MAX_EXTENSION_BYTES {
                bail!("it has a tar extension header of {size} bytes, more than a path needs");
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            pending.add(kind, &data)?;
            continue;
        }
        let described = std::mem::take(&mut pending);
        let path = match described.path() {
            Some(path) => path,
            None => entry.path()?.display().to_string(),
        };
        visit(&path, &mut entry)?;
    }
    Ok(())
}

/// Read a package file into the entry it gets in `repo.db` and `repo.files`:
/// its `.PKGINFO`, the files it installs, its size and its checksums, plus a
/// detached `<file>.sig` beside it if there is one.
///
/// Reads the whole archive, streamed, within the bounds above.
pub fn describe_package(pkgfile: &Path) -> anyhow::Result<PackageEntry> {
    let pkgpath = pkgfile.display();
    let csize = fs::metadata(pkgfile)?.len();
    let limit = decompressed_limit(csize);
    // One byte past the limit, so that running out means it went past.
    let mut decompressed = decompress(pkgfile)?.take(limit.saturating_add(1));
    let contents = read_contents(&mut Archive::new(&mut decompressed));
    if decompressed.limit() == 0 {
        bail!(
            "it decompresses to more than {limit} bytes, {MAX_EXPANSION} times its own size: \
             no real package expands that far"
        );
    }
    let Contents { mut pkginfo, files } = contents?;

    if !pkginfo.valid() {
        error!("Invalid package file '{pkgpath}'.");
        bail!("Invalid package file");
    }

    pkginfo.set_signature(pkgfile)?;

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
    desc.sha256sum.clone_from(&sha256sum);

    Ok(PackageEntry {
        filename,
        dir_name,
        sha256sum,
        desc: desc.to_string(),
        files,
    })
}

struct Contents {
    /// Its `.PKGINFO`; empty, and so not [`Pkginfo::valid`], when it has
    /// none.
    pkginfo: Pkginfo,
    files: String,
}

fn read_contents<R: Read>(archive: &mut Archive<R>) -> anyhow::Result<Contents> {
    let mut listing = Listing::default();
    let mut pkginfo: Option<Pkginfo> = None;
    walk(archive, |path, entry| {
        if path == ".PKGINFO" {
            // A second would be parsed over the first, past its size bound,
            // mixing two packages' fields.
            if pkginfo.is_some() {
                bail!("it has more than one .PKGINFO");
            }
            let mut parsed = Pkginfo::new();
            parsed.parse(entry)?;
            pkginfo = Some(parsed);
        } else if !path.starts_with('.') {
            listing.push(path)?;
        }
        Ok(())
    })?;
    Ok(Contents {
        pkginfo: pkginfo.unwrap_or_default(),
        files: listing.into_files(),
    })
}

#[derive(Default)]
struct Listing {
    paths: Vec<String>,
    /// What the list takes written out, a line per path.
    bytes: usize,
}

impl Listing {
    fn push(&mut self, path: &str) -> anyhow::Result<()> {
        if self.paths.len() == MAX_PATHS {
            bail!("it lists more than {MAX_PATHS} paths");
        }
        self.bytes += path.len() + 1;
        if self.bytes > MAX_LISTING_BYTES {
            bail!("its file list is longer than {MAX_LISTING_BYTES} bytes");
        }
        self.paths.push(path.to_string());
        Ok(())
    }

    fn into_files(mut self) -> String {
        self.paths.sort_unstable();
        let mut files = String::with_capacity("%FILES%\n".len() + self.bytes);
        files.push_str("%FILES%\n");
        for (i, path) in self.paths.iter().enumerate() {
            if i > 0 {
                files.push('\n');
            }
            files.push_str(path);
        }
        files
    }
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
    use std::io::Write;

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

    /// One member of a test archive: a header of `kind` for `path`, and its
    /// data.
    struct Member<'a> {
        kind: tar::EntryType,
        path: &'a str,
        data: &'a [u8],
    }

    /// A regular file.
    fn file<'a>(path: &'a str, data: &'a [u8]) -> Member<'a> {
        Member {
            kind: tar::EntryType::Regular,
            path,
            data,
        }
    }

    /// An extension header, describing the member after it.
    fn extension(kind: tar::EntryType, data: &[u8]) -> Member<'_> {
        let path = if kind.is_pax_local_extensions() {
            "PaxHeaders/x"
        } else {
            "././@LongLink"
        };
        Member { kind, path, data }
    }

    const PKGINFO: &[u8] = b"pkgname = demo\npkgver = 1.0-1\n";

    fn header(kind: tar::EntryType, path: &str, size: u64) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_path(path).unwrap();
        header.set_size(size);
        header.set_mode(0o644);
        header.set_cksum();
        header
    }

    /// `members` as tar, each header followed by its data, with no end
    /// marker: an archive to be continued.
    fn tar_members(members: &[Member<'_>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for Member { kind, path, data } in members {
            bytes.extend_from_slice(header(*kind, path, data.len() as u64).as_bytes());
            bytes.extend_from_slice(data);
            bytes.resize(bytes.len().next_multiple_of(512), 0);
        }
        bytes
    }

    /// A zstd archive whose members are `members`, in order.
    fn archive(members: &[Member<'_>]) -> Vec<u8> {
        let mut bytes = tar_members(members);
        bytes.extend_from_slice(&[0; 1024]);
        zstd::stream::encode_all(&bytes[..], 3).unwrap()
    }

    /// A zstd frame of `prefix`, then `zeros` zero bytes as RLE blocks: four
    /// bytes of the frame for every 128 KiB of them.
    fn zstd_with_zeros(prefix: &[u8], zeros: u64) -> Vec<u8> {
        const BLOCK: usize = 128 << 10;
        // The magic number, no flags, a 128 KiB window.
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x38];
        let block_header = |frame: &mut Vec<u8>, rle: bool, size: usize, last: bool| {
            let header =
                (u32::try_from(size).unwrap() << 3) | (u32::from(rle) << 1) | u32::from(last);
            frame.extend_from_slice(&header.to_le_bytes()[..3]);
        };
        for chunk in prefix.chunks(BLOCK) {
            block_header(&mut frame, false, chunk.len(), false);
            frame.extend_from_slice(chunk);
        }
        let mut left = zeros;
        while left > 0 {
            let size = left.min(BLOCK as u64);
            left -= size;
            block_header(&mut frame, true, usize::try_from(size).unwrap(), left == 0);
            frame.push(0);
        }
        frame
    }

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    /// `describe_package` of an archive of `members`.
    fn describe_members(members: &[Member<'_>]) -> anyhow::Result<PackageEntry> {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "demo-1.0-1-x86_64.pkg.tar.zst", &archive(members));
        describe_package(&path)
    }

    /// A GNU long name claiming 32 MiB is refused from its header, unread.
    #[test]
    fn an_oversized_extension_header_is_refused_unread() {
        let huge = vec![b'a'; 32 << 20];
        let bytes = archive(&[
            extension(tar::EntryType::GNULongName, &huge),
            file(".PKGINFO", PKGINFO),
        ]);
        assert!(bytes.len() < 64 * 1024, "{} bytes", bytes.len());
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "demo-1.0-1-x86_64.pkg.tar.zst", &bytes);

        let described = describe_package(&path).unwrap_err().to_string();
        assert!(described.contains("extension header"), "{described}");
    }

    /// Long paths still come out whole: from a GNU long name, and from a pax
    /// record, the way bsdtar writes them.
    #[test]
    fn long_paths_are_listed_whole() {
        let long = format!("usr/share/{}/file", "d".repeat(150));
        let pax = pax_record("path", &long);
        let other = format!("usr/lib/{}/other", "e".repeat(150));
        let entry = describe_members(&[
            file(".PKGINFO", PKGINFO),
            extension(tar::EntryType::XHeader, pax.as_bytes()),
            file("usr/share/short", b"x"),
            extension(tar::EntryType::GNULongName, other.as_bytes()),
            file("usr/lib/short", b"y"),
        ])
        .unwrap();
        assert_eq!(entry.files, format!("%FILES%\n{other}\n{long}"));
    }

    /// One pax record, `key=value`, length-prefixed as the format has it.
    fn pax_record(key: &str, value: &str) -> String {
        let unprefixed = key.len() + value.len() + 3;
        let mut len = unprefixed;
        while len != unprefixed + len.to_string().len() {
            len = unprefixed + len.to_string().len();
        }
        format!("{len} {key}={value}\n")
    }

    /// A second `.PKGINFO` would be read over the first.
    #[test]
    fn two_pkginfos_are_refused() {
        let error = describe_members(&[
            file(".PKGINFO", PKGINFO),
            file(
                ".PKGINFO",
                b"pkgname = demo\npkgver = 1.0-1\nprovides = openssh\n",
            ),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("more than one .PKGINFO"), "{error}");
    }

    /// A flood of long names, each a path of its own, stops at the list's
    /// limit rather than holding every one of them.
    #[test]
    fn a_file_list_past_its_limit_is_refused() {
        let name = "n".repeat(60_000);
        let one = tar_members(&[
            extension(tar::EntryType::GNULongName, name.as_bytes()),
            file("usr/x", b""),
        ]);
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        encoder
            .write_all(&tar_members(&[file(".PKGINFO", PKGINFO)]))
            .unwrap();
        for _ in 0..=MAX_LISTING_BYTES / name.len() {
            encoder.write_all(&one).unwrap();
        }
        let mut bytes = encoder.finish().unwrap();
        // Room in its size to expand that far. Reading stops before the
        // padding.
        bytes.resize(1 << 20, 0);
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "demo-1.0-1-x86_64.pkg.tar.zst", &bytes);

        let error = describe_package(&path).unwrap_err().to_string();
        assert!(error.contains("file list is longer than"), "{error}");
    }

    /// The list may hold its limit in paths, and not one more. Built directly:
    /// an archive of that many members is too slow to read in a unit test.
    #[test]
    fn a_file_list_of_too_many_paths_is_refused() {
        let mut listing = Listing {
            paths: vec![String::new(); MAX_PATHS - 1],
            bytes: 0,
        };
        listing.push("usr/a").unwrap();
        let error = listing.push("usr/b").unwrap_err().to_string();
        assert!(error.contains("more than 4000000 paths"), "{error}");
    }

    /// A small file expanding to gigabytes stops at its bound, not at its end.
    #[test]
    fn an_archive_expanding_too_far_is_stopped_early() {
        let mut prefix = tar_members(&[file(".PKGINFO", PKGINFO)]);
        prefix.extend_from_slice(header(tar::EntryType::Regular, "usr/huge", 1 << 40).as_bytes());
        let bytes = zstd_with_zeros(&prefix, 4 << 30);
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "demo-1.0-1-x86_64.pkg.tar.zst", &bytes);

        let error = describe_package(&path).unwrap_err().to_string();
        let limit = bytes.len() as u64 * MAX_EXPANSION;
        assert!(
            error.contains(&format!("decompresses to more than {limit} bytes")),
            "{error}"
        );
    }

    /// However well a small package compresses, it may expand to the floor.
    #[test]
    fn a_small_package_may_expand_to_the_floor() {
        let zeros = 8 << 20;
        let mut prefix = tar_members(&[file(".PKGINFO", PKGINFO)]);
        prefix.extend_from_slice(header(tar::EntryType::Regular, "usr/zeros", zeros).as_bytes());
        let bytes = zstd_with_zeros(&prefix, zeros + 1024);
        assert!((bytes.len() as u64) * MAX_EXPANSION < zeros);
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "demo-1.0-1-x86_64.pkg.tar.zst", &bytes);

        assert_eq!(describe_package(&path).unwrap().files, "%FILES%\nusr/zeros");
    }

    #[test]
    fn the_decompressed_limit_scales_with_the_file() {
        assert_eq!(decompressed_limit(1), MIN_DECOMPRESSED_LIMIT);
        assert_eq!(decompressed_limit(1 << 20), 1 << 30);
        assert_eq!(decompressed_limit(u64::MAX), u64::MAX);
    }

    /// A detached signature beside a package goes into its entry as
    /// `%PGPSIG%` and reads back from the database as the same bytes; without
    /// one, the entry carries no `%PGPSIG%` at all.
    #[test]
    fn a_detached_signature_round_trips_through_the_database() {
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
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("demo-1.0-1-x86_64.pkg.tar.zst");
        fs::write(&path, zstd::stream::encode_all(&bytes[..], 0).unwrap()).unwrap();

        let unsigned = describe_package(&path).unwrap();
        assert!(!unsigned.desc.contains("%PGPSIG%"));

        let signature = [0x88_u8, 0x75, 0x04, 0x00, 0x16, 0x0a, 0x00, 0x1d];
        fs::write(
            tmp.path().join("demo-1.0-1-x86_64.pkg.tar.zst.sig"),
            signature,
        )
        .unwrap();
        let signed = describe_package(&path).unwrap();
        assert!(signed.desc.contains("%PGPSIG%\niHUEABYKAB0=\n"));

        let (db, files) = (tmp.path().join("repo.db"), tmp.path().join("repo.files"));
        crate::write_updated_databases(
            &tmp.path().join("none.db"),
            &tmp.path().join("none.files"),
            &[],
            &[signed],
            &db,
            &files,
        )
        .unwrap();
        let listed = crate::listed_packages(&db).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].signature.as_deref(), Some(&signature[..]));
        assert_eq!(listed[0].sha256sum.as_deref().map(str::len), Some(64));
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
