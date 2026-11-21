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
    /// The `desc` file both databases carry.
    pub(crate) desc: String,
    /// The `files` file only `repo.files` carries.
    pub(crate) files: String,
    /// What its `.PKGINFO` says it is.
    pub meta: PackageMeta,
}

/// What a package's `.PKGINFO` says it is, of the fields that decide where
/// it belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageMeta {
    pub pkgname: String,
    /// `[epoch:]pkgver-pkgrel`, the way a filename spells it.
    pub version: String,
    /// `None` when the `.PKGINFO` names none.
    pub arch: Option<String>,
}

impl From<&Pkginfo> for PackageMeta {
    fn from(pkginfo: &Pkginfo) -> Self {
        Self {
            pkgname: pkginfo.pkgname.clone(),
            version: pkginfo.pkgver.clone(),
            arch: Some(pkginfo.arch.clone()).filter(|arch| !arch.is_empty()),
        }
    }
}

/// The largest tar extension header -- a GNU long name or long link, a pax
/// record -- an archive may hold. A real one is a path, a few hundred bytes.
const MAX_EXTENSION_BYTES: u64 = 64 * 1024;

/// The most memory xz's decoder may take. An xz stream names the dictionary
/// it needs, up to 1.5 GiB, and the decoder allocates all of it up front; a
/// real package's is at most `xz -9`'s 64 MiB.
const MAX_XZ_MEMORY: u64 = 256 << 20;

/// What reading one package archive may cost.
///
/// The archive is whoever built the package's, and compressed: a few KiB of
/// zstd or xz can expand to terabytes, and a tar header can say anything.
/// Without these, a small file could keep the server reading for hours, or
/// listing more paths than it has memory for.
///
/// They bound one read, not the server: a package near every limit holds
/// about 650 MB while it is described, and several packages can be published
/// at once. Nor do they bound a read's time outright, only relative to the
/// package file's size: a larger file may take longer.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// Tar headers, extension headers included. The package with the most
    /// files seen, Unreal Engine's, has about 540,000 members, some with a
    /// pax header of their own.
    headers: u64,
    /// Bytes of the file list, a line per path: what `repo.files` holds for
    /// the package, and what listing it holds in memory, twice over while it
    /// is written. Unreal Engine's is about 60 MB.
    listing: usize,
    /// How many times the package file's own size its contents may
    /// decompress to. This is what bounds the time reading one takes, by
    /// the file's own size rather than by anything the archive says about
    /// itself. Real packages stay under 100 -- the most seen is a firmware
    /// package of zero-padded flash images.
    expansion: u64,
    /// What a package may decompress to however small its file is: a ratio
    /// says little about a package of a few KiB.
    decompressed_floor: u64,
}

impl Limits {
    const DEFAULT: Self = Self {
        headers: 4_000_000,
        listing: 256 << 20,
        expansion: 1024,
        decompressed_floor: 64 << 20,
    };

    /// The most a package file of `csize` bytes may decompress to.
    fn decompressed(&self, csize: u64) -> u64 {
        csize
            .saturating_mul(self.expansion)
            .max(self.decompressed_floor)
    }
}

/// A package's contents, decompressed as its extension says.
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

/// What the extension headers read since the last member say about the next
/// one.
#[derive(Default)]
struct Pending {
    /// A GNU long name.
    long_name: Option<String>,
    /// A pax header's `path`.
    pax_path: Option<String>,
}

impl Pending {
    /// Take in the extension header `kind`, holding `data`.
    ///
    /// A GNU long link only ever names a symlink's target, and a pax global
    /// header describes no one member: nothing here needs either.
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

    /// The path the member is stored under, if an extension header says.
    fn path(self) -> Option<String> {
        self.pax_path.or(self.long_name)
    }
}

/// Whether `kind` is a header describing the member after it rather than a
/// member.
fn is_extension(kind: tar::EntryType) -> bool {
    kind.is_gnu_longname()
        || kind.is_gnu_longlink()
        || kind.is_pax_local_extensions()
        || kind.is_pax_global_extensions()
}

/// Visit an archive's members, each with the path it is stored under.
///
/// Read raw, with GNU long names and pax paths applied here: the tar crate
/// resolves those itself by reading each extension header whole into memory,
/// however large the header says it is, and a few KiB of compressed archive
/// can say gigabytes. Here one larger than [`MAX_EXTENSION_BYTES`] is refused
/// before any of it is read.
fn walk<R: Read>(
    archive: &mut Archive<R>,
    limits: &Limits,
    mut visit: impl FnMut(&str, &mut tar::Entry<'_, R>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut pending = Pending::default();
    let mut headers: u64 = 0;
    // Raw, each member's size is its ustar header's and a pax `size` record
    // is ignored: bsdtar, which makepkg archives with, always writes the
    // header's size field too.
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        headers += 1;
        if headers > limits.headers {
            bail!("it has more than {} tar headers", limits.headers);
        }
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
/// Reads the whole archive, streamed, within [`Limits::DEFAULT`]: a package
/// that goes past one fails here, before anything is changed.
pub fn describe_package(pkgfile: &Path) -> anyhow::Result<PackageEntry> {
    describe(pkgfile, &Limits::DEFAULT)
}

/// [`describe_package`], within `limits`.
fn describe(pkgfile: &Path, limits: &Limits) -> anyhow::Result<PackageEntry> {
    let pkgpath = pkgfile.display();
    let csize = fs::metadata(pkgfile)?.len();
    let limit = limits.decompressed(csize);
    // One byte past the limit, so that running out means it went past: the
    // archive then ends early, wherever it was, skipping a member's data
    // reads it too.
    let mut decompressed = decompress(pkgfile)?.take(limit.saturating_add(1));
    let contents = read_contents(&mut Archive::new(&mut decompressed), limits);
    if decompressed.limit() == 0 {
        bail!(
            "it decompresses to more than {limit} bytes, {} times its own size: \
             no real package expands that far",
            limits.expansion
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
    let meta = PackageMeta::from(&pkginfo);

    let mut desc = Desc::from(pkginfo);
    desc.filename.clone_from(&filename);
    desc.md5sum = md5sum;
    desc.csize = csize.to_string();
    desc.sha256sum = sha256sum;

    Ok(PackageEntry {
        filename,
        dir_name,
        desc: desc.to_string(),
        files,
        meta,
    })
}

/// What an archive holds, of what its database entries need.
struct Contents {
    /// Its `.PKGINFO`; empty, and so not [`Pkginfo::valid`], when it has
    /// none.
    pkginfo: Pkginfo,
    /// The `files` file of its `repo.files` entry.
    files: String,
}

/// Read an archive's `.PKGINFO`, and list the paths it installs.
fn read_contents<R: Read>(archive: &mut Archive<R>, limits: &Limits) -> anyhow::Result<Contents> {
    let mut listing = Listing::new(limits.listing);
    let mut pkginfo: Option<Pkginfo> = None;
    walk(archive, limits, |path, entry| {
        if path == ".PKGINFO" {
            // A second one would be parsed over the first: the entry's name,
            // version and pkgbase from one, what it provides from the other.
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

/// The paths a package installs, as its `repo.files` entry lists them.
struct Listing {
    paths: Vec<String>,
    /// What the list takes written out, a line per path.
    bytes: usize,
    limit: usize,
}

impl Listing {
    fn new(limit: usize) -> Self {
        Self {
            paths: Vec::new(),
            bytes: 0,
            limit,
        }
    }

    /// Add `path`, unless it would make the list longer than its limit.
    ///
    /// Each path is a line of the list, and pacman reads a blank line as
    /// the end of it, and what follows as another section: an empty path,
    /// or one holding a line break, is refused rather than written.
    fn push(&mut self, path: &str) -> anyhow::Result<()> {
        if path.is_empty() {
            bail!("it has a member with no path");
        }
        if path.contains('\n') {
            bail!("it has a member whose path holds a line break");
        }
        self.bytes += path.len() + 1;
        if self.bytes > self.limit {
            bail!("its file list is longer than {} bytes", self.limit);
        }
        self.paths.push(path.to_string());
        Ok(())
    }

    /// The `files` file: `%FILES%`, then every path, sorted.
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

    /// `describe` of an archive of `members`, within `limits`.
    fn describe_members(members: &[Member<'_>], limits: &Limits) -> anyhow::Result<PackageEntry> {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "demo-1.0-1-x86_64.pkg.tar.zst", &archive(members));
        describe(&path, limits)
    }

    /// A GNU long name claiming gigabytes is refused from its header, before
    /// any of it is read -- a few hundred KiB compressed would otherwise make
    /// the reader allocate all of it.
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
        let entry = describe_members(
            &[
                file(".PKGINFO", PKGINFO),
                extension(tar::EntryType::XHeader, pax.as_bytes()),
                file("usr/share/short", b"x"),
                extension(tar::EntryType::GNULongName, other.as_bytes()),
                file("usr/lib/short", b"y"),
            ],
            &Limits::DEFAULT,
        )
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
        let error = describe_members(
            &[
                file(".PKGINFO", PKGINFO),
                file(
                    ".PKGINFO",
                    b"pkgname = demo\npkgver = 1.0-1\nprovides = openssh\n",
                ),
            ],
            &Limits::DEFAULT,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("more than one .PKGINFO"), "{error}");
    }

    /// A flood of long names, each a path of its own, stops at the listing's
    /// limit rather than holding every one of them.
    #[test]
    fn a_file_list_past_its_limit_is_refused() {
        let limits = Limits {
            listing: 4096,
            ..Limits::DEFAULT
        };
        let name = "n".repeat(1000);
        let mut members = vec![file(".PKGINFO", PKGINFO)];
        for _ in 0..5 {
            members.push(extension(tar::EntryType::GNULongName, name.as_bytes()));
            members.push(file("usr/x", b""));
        }
        let error = describe_members(&members, &limits).unwrap_err().to_string();
        assert!(error.contains("file list is longer than 4096"), "{error}");

        members.truncate(1 + 2 * 4);
        assert!(describe_members(&members, &limits).is_ok());
    }

    #[test]
    fn an_archive_of_too_many_headers_is_refused() {
        let limits = Limits {
            headers: 8,
            ..Limits::DEFAULT
        };
        let mut members = vec![file(".PKGINFO", PKGINFO)];
        members.extend((0..7).map(|_| file("usr/x", b"")));
        assert!(describe_members(&members, &limits).is_ok());

        members.push(file("usr/x", b""));
        let error = describe_members(&members, &limits).unwrap_err().to_string();
        assert!(error.contains("more than 8 tar headers"), "{error}");
    }

    /// A small file expanding to gigabytes is stopped once it has expanded
    /// as far as its size allows, not read to its end: here 4 GiB of zeros
    /// behind a member said to be a terabyte, from a 128 KiB file.
    #[test]
    fn an_archive_expanding_too_far_is_stopped_early() {
        let mut prefix = tar_members(&[file(".PKGINFO", PKGINFO)]);
        prefix.extend_from_slice(header(tar::EntryType::Regular, "usr/huge", 1 << 40).as_bytes());
        let bytes = zstd_with_zeros(&prefix, 4 << 30);
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "demo-1.0-1-x86_64.pkg.tar.zst", &bytes);

        let error = describe_package(&path).unwrap_err().to_string();
        let limit = bytes.len() as u64 * Limits::DEFAULT.expansion;
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
        assert!((bytes.len() as u64) * Limits::DEFAULT.expansion < zeros);
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "demo-1.0-1-x86_64.pkg.tar.zst", &bytes);

        assert_eq!(describe_package(&path).unwrap().files, "%FILES%\nusr/zeros");
    }

    #[test]
    fn the_decompressed_limit_scales_with_the_file() {
        let limits = Limits::DEFAULT;
        assert_eq!(limits.decompressed(1), limits.decompressed_floor);
        assert_eq!(limits.decompressed(1 << 20), 1 << 30);
        assert_eq!(limits.decompressed(u64::MAX), u64::MAX);
    }

    /// A path is a line of `repo.files`, where a blank line ends a section
    /// and the next line starts another.
    #[test]
    fn a_path_that_would_break_the_file_list_is_refused() {
        for name in [&b"\0"[..], b"usr/a\n\n%BACKUP%\nusr/b"] {
            let error = describe_members(
                &[
                    file(".PKGINFO", PKGINFO),
                    extension(tar::EntryType::GNULongName, name),
                    file("usr/x", b""),
                ],
                &Limits::DEFAULT,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("path"), "{error}");
        }
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
