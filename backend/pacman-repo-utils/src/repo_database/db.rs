use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use tar::{Archive, Builder, Header};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::describe::PackageEntry;
use base64::Engine;
use base64::engine::general_purpose;

/// The directory a package file's entry occupies in the databases.
///
/// `hello-2.12.1-1-x86_64.pkg.tar.zst` is `hello-2.12.1-1`: everything before
/// the architecture, which is the last `-` field.
fn entry_dir_for_filename(filename: &str) -> &str {
    filename.rsplit_once('-').map_or(filename, |(dir, _)| dir)
}

/// Write `output` as a copy of the archive at `input` (or of an empty one, if
/// there is none), without the entries `superseded` selects and with whatever
/// `fill` appends. `input` is only read.
///
/// `superseded` takes a [`Path`], not a `&str`, so it matches whole path
/// components rather than bytes. Entry paths here are `{pkgname}-{ver}`
/// directories, where one name is routinely a byte prefix of another: with
/// `str::starts_with`, removing `foo-1.0-1` also drops the `foo-1.0-10` a
/// rebuild just added.
fn rewrite_archive<F, G>(input: &Path, output: &Path, superseded: F, fill: G) -> anyhow::Result<()>
where
    F: Fn(&Path) -> bool,
    G: FnOnce(&mut Builder<GzEncoder<File>>) -> anyhow::Result<()>,
{
    let encoder = GzEncoder::new(File::create(output)?, Compression::default());
    let mut builder = Builder::new(encoder);

    if input.exists() {
        let mut archive = Archive::new(GzDecoder::new(BufReader::new(File::open(input)?)));
        for entry in archive.entries()? {
            let mut entry = entry?;
            if superseded(&entry.path()?) {
                continue;
            }
            let header = entry.header().clone();
            builder.append(&header, &mut entry)?;
        }
    }

    fill(&mut builder)?;
    let file = builder.into_inner()?.finish()?;
    // On disk before anyone renames it into place.
    file.sync_all()?;
    Ok(())
}

/// Append the directory of a package's entry.
fn append_dir<W: Write>(builder: &mut Builder<W>, dir_name: &str) -> anyhow::Result<()> {
    let mut header = Header::new_gnu();
    header.set_path(dir_name)?;
    header.set_entry_type(tar::EntryType::Directory);
    header.set_mode(0o755);
    header.set_size(0);
    header.set_cksum();
    builder.append(&header, io::empty())?;
    Ok(())
}

/// Append one file of a package's entry, in its directory.
fn append_file<W: Write>(
    builder: &mut Builder<W>,
    dir_name: &str,
    file_name: &str,
    content: &str,
) -> anyhow::Result<()> {
    let mut header = Header::new_gnu();
    header.set_path(format!("{dir_name}/{file_name}"))?;
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, content.as_bytes())?;
    Ok(())
}

/// Write the updated `repo.db` and `repo.files` beside the current ones: the
/// entries of the package files named in `remove` dropped, and `add` added,
/// replacing any entry already at the same `{pkgname}-{pkgver}`.
///
/// The current databases are only read. Nothing is visible to a client until
/// the caller renames `db_out` and `files_out` over them, so a failure here, or
/// a decision not to go ahead, costs nothing but the files it wrote.
pub fn write_updated_databases(
    db: &Path,
    files: &Path,
    remove: &[String],
    add: &[PackageEntry],
    db_out: &Path,
    files_out: &Path,
) -> anyhow::Result<()> {
    let superseded = |path: &Path| {
        remove
            .iter()
            .map(|filename| entry_dir_for_filename(filename))
            .chain(add.iter().map(|entry| entry.dir_name.as_str()))
            .any(|dir| path.starts_with(dir))
    };

    rewrite_archive(db, db_out, superseded, |builder| {
        for entry in add {
            append_dir(builder, &entry.dir_name)?;
            append_file(builder, &entry.dir_name, "desc", &entry.desc)?;
        }
        Ok(())
    })?;
    rewrite_archive(files, files_out, superseded, |builder| {
        for entry in add {
            append_dir(builder, &entry.dir_name)?;
            append_file(builder, &entry.dir_name, "desc", &entry.desc)?;
            append_file(builder, &entry.dir_name, "files", &entry.files)?;
        }
        Ok(())
    })
}

/// The package files a `repo.db` lists, by the `%FILENAME%` of each entry.
pub fn listed_filenames(db_archive: &Path) -> anyhow::Result<HashSet<String>> {
    Ok(listed_packages(db_archive)?
        .into_iter()
        .map(|listed| listed.filename)
        .collect())
}

/// One entry of a `repo.db`: the file it lists, and what it says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPackage {
    /// `%FILENAME%`.
    pub filename: String,
    /// `%SHA256SUM%`, which tells one build of a filename from another.
    pub sha256sum: Option<String>,
    /// `%PGPSIG%`, decoded: the package's detached signature, when the entry
    /// carries one.
    pub signature: Option<Vec<u8>>,
}

/// Every entry a `repo.db` lists.
///
/// A `%PGPSIG%` that is not base64 reads as no signature: pacman could not
/// use it either.
pub fn listed_packages(db_archive: &Path) -> anyhow::Result<Vec<ListedPackage>> {
    let mut listed = Vec::new();
    let mut archive = Archive::new(GzDecoder::new(BufReader::new(File::open(db_archive)?)));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name().and_then(|n| n.to_str()) != Some("desc") {
            continue;
        }
        let mut desc = String::new();
        entry.read_to_string(&mut desc)?;
        if let Some(package) = listed_package(&desc) {
            listed.push(package);
        }
    }
    Ok(listed)
}

/// What the listing reads from one `desc`, if it names a file.
fn listed_package(desc: &str) -> Option<ListedPackage> {
    let mut filename = None;
    let mut sha256sum = None;
    let mut signature = None;
    let mut lines = desc.lines();
    while let Some(line) = lines.next() {
        let field = match line {
            "%FILENAME%" => &mut filename,
            "%SHA256SUM%" => &mut sha256sum,
            "%PGPSIG%" => &mut signature,
            _ => continue,
        };
        *field = lines.next().map(|value| value.trim().to_string());
    }
    Some(ListedPackage {
        filename: filename?,
        sha256sum,
        signature: signature.and_then(|encoded| general_purpose::STANDARD.decode(encoded).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(filename: &str, dir_name: &str) -> PackageEntry {
        PackageEntry {
            filename: filename.to_string(),
            dir_name: dir_name.to_string(),
            sha256sum: String::new(),
            desc: format!("%FILENAME%\n{filename}\n"),
            files: "%FILES%\nusr/bin/x".to_string(),
            meta: crate::PackageMeta {
                pkgname: dir_name.to_string(),
                pkgbase: None,
                version: String::new(),
                arch: None,
                description: None,
                url: None,
                licenses: Vec::new(),
                provides: Vec::new(),
            },
        }
    }

    /// Every entry path in the archive at `path`.
    fn entry_paths(path: &Path) -> Vec<String> {
        let mut data = Vec::new();
        File::open(path).unwrap().read_to_end(&mut data).unwrap();
        let mut archive = Archive::new(GzDecoder::new(io::Cursor::new(data)));
        archive
            .entries()
            .unwrap()
            .flatten()
            .map(|e| e.path().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// Apply one update in place, the way a caller renames the result over.
    fn update(dir: &Path, remove: &[&str], add: &[PackageEntry]) {
        let (db, files) = (dir.join("repo.db.tar.gz"), dir.join("repo.files.tar.gz"));
        let (db_out, files_out) = (dir.join(".db.next"), dir.join(".files.next"));
        let remove: Vec<String> = remove.iter().map(ToString::to_string).collect();
        write_updated_databases(&db, &files, &remove, add, &db_out, &files_out).unwrap();
        std::fs::rename(db_out, db).unwrap();
        std::fs::rename(files_out, files).unwrap();
    }

    /// Removing one package must not take its version-prefixed neighbours with
    /// it. `foo-1.0-1` is a byte prefix of `foo-1.0-10`, so a `str::starts_with`
    /// predicate deletes the entry a rebuild just added — the package silently
    /// disappears from the repository database while its file stays on disk.
    #[test]
    fn removing_a_pkgrel_does_not_remove_its_longer_neighbour() {
        let tmp = tempfile::tempdir().unwrap();
        update(
            tmp.path(),
            &[],
            &[
                entry("foo-1.0-1-x86_64.pkg.tar.zst", "foo-1.0-1"),
                entry("foo-1.0-10-x86_64.pkg.tar.zst", "foo-1.0-10"),
            ],
        );

        update(tmp.path(), &["foo-1.0-1-x86_64.pkg.tar.zst"], &[]);

        let paths = entry_paths(&tmp.path().join("repo.db.tar.gz"));
        assert!(
            paths.iter().any(|p| p.starts_with("foo-1.0-10")),
            "the rebuilt package was removed along with the old pkgrel: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p == "foo-1.0-1/desc"),
            "the old pkgrel survived removal: {paths:?}"
        );
    }

    /// Re-adding a package replaces its entries rather than stacking
    /// duplicates, which is what adding over the same directory is for.
    #[test]
    fn re_adding_replaces_rather_than_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let foo = entry("foo-1.0-1-x86_64.pkg.tar.zst", "foo-1.0-1");
        update(tmp.path(), &[], std::slice::from_ref(&foo));
        update(tmp.path(), &[], std::slice::from_ref(&foo));

        let db = entry_paths(&tmp.path().join("repo.db.tar.gz"));
        assert_eq!(db, ["foo-1.0-1", "foo-1.0-1/desc"]);
        let files = entry_paths(&tmp.path().join("repo.files.tar.gz"));
        assert_eq!(files, ["foo-1.0-1", "foo-1.0-1/desc", "foo-1.0-1/files"]);
    }

    /// A new version in, the old one out, in one rewrite: what publishing a
    /// build does, and what two separate rewrites could leave half-done.
    #[test]
    fn one_update_adds_and_removes_together() {
        let tmp = tempfile::tempdir().unwrap();
        update(
            tmp.path(),
            &[],
            &[
                entry("foo-1.0-1-x86_64.pkg.tar.zst", "foo-1.0-1"),
                entry("bar-2.0-1-x86_64.pkg.tar.zst", "bar-2.0-1"),
            ],
        );

        update(
            tmp.path(),
            &["foo-1.0-1-x86_64.pkg.tar.zst"],
            &[entry("foo-1.1-1-x86_64.pkg.tar.zst", "foo-1.1-1")],
        );

        let db = entry_paths(&tmp.path().join("repo.db.tar.gz"));
        assert_eq!(
            db,
            ["bar-2.0-1", "bar-2.0-1/desc", "foo-1.1-1", "foo-1.1-1/desc"]
        );
    }

    /// The current databases are only read: writing an update that is never
    /// renamed into place changes nothing.
    #[test]
    fn an_update_not_renamed_in_leaves_the_databases_alone() {
        let tmp = tempfile::tempdir().unwrap();
        update(
            tmp.path(),
            &[],
            &[entry("foo-1.0-1-x86_64.pkg.tar.zst", "foo-1.0-1")],
        );
        let before = std::fs::read(tmp.path().join("repo.db.tar.gz")).unwrap();

        write_updated_databases(
            &tmp.path().join("repo.db.tar.gz"),
            &tmp.path().join("repo.files.tar.gz"),
            &["foo-1.0-1-x86_64.pkg.tar.zst".to_string()],
            &[],
            &tmp.path().join(".db.next"),
            &tmp.path().join(".files.next"),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(tmp.path().join("repo.db.tar.gz")).unwrap(),
            before
        );
    }

    /// What a database lists is what its entries name, after any update.
    #[test]
    fn a_database_lists_the_files_its_entries_name() {
        let tmp = tempfile::tempdir().unwrap();
        update(
            tmp.path(),
            &[],
            &[
                entry("foo-1.0-1-x86_64.pkg.tar.zst", "foo-1.0-1"),
                entry("bar-2.0-1-x86_64.pkg.tar.zst", "bar-2.0-1"),
            ],
        );
        update(tmp.path(), &["bar-2.0-1-x86_64.pkg.tar.zst"], &[]);

        let listed = listed_filenames(&tmp.path().join("repo.db.tar.gz")).unwrap();
        assert_eq!(
            listed,
            HashSet::from(["foo-1.0-1-x86_64.pkg.tar.zst".to_string()])
        );
    }

    /// The listing reads what each entry says about its file, the signature
    /// decoded, and an entry without one reads as unsigned.
    #[test]
    fn the_listing_carries_each_entrys_checksum_and_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let signed = PackageEntry {
            filename: "foo-1.0-1-x86_64.pkg.tar.zst".to_string(),
            dir_name: "foo-1.0-1".to_string(),
            sha256sum: "abc".to_string(),
            desc:
                "%FILENAME%\nfoo-1.0-1-x86_64.pkg.tar.zst\n\n%SHA256SUM%\nabc\n\n%PGPSIG%\nc2ln\n\n"
                    .to_string(),
            files: "%FILES%\nusr/bin/foo".to_string(),
            meta: entry("foo-1.0-1-x86_64.pkg.tar.zst", "foo-1.0-1").meta,
        };
        update(
            tmp.path(),
            &[],
            &[signed, entry("bar-1.0-1-x86_64.pkg.tar.zst", "bar-1.0-1")],
        );

        let mut listed = listed_packages(&tmp.path().join("repo.db.tar.gz")).unwrap();
        listed.sort_by(|a, b| a.filename.cmp(&b.filename));
        assert_eq!(
            listed,
            [
                ListedPackage {
                    filename: "bar-1.0-1-x86_64.pkg.tar.zst".to_string(),
                    sha256sum: None,
                    signature: None,
                },
                ListedPackage {
                    filename: "foo-1.0-1-x86_64.pkg.tar.zst".to_string(),
                    sha256sum: Some("abc".to_string()),
                    signature: Some(b"sig".to_vec()),
                },
            ]
        );
    }

    #[test]
    fn a_package_filename_maps_to_its_entry_directory() {
        assert_eq!(
            entry_dir_for_filename("hello-2.12.1-1-x86_64.pkg.tar.zst"),
            "hello-2.12.1-1"
        );
        assert_eq!(
            entry_dir_for_filename("lib32-glibc-2.39-1-any.pkg.tar.zst"),
            "lib32-glibc-2.39-1"
        );
    }
}
