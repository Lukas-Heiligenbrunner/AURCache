use anyhow::{anyhow, bail};

use crate::repo_database::db::add_to_db_file;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io;
use std::io::{BufReader, Read};
use std::path::Path;
use std::str::FromStr;
use alpm_pkginfo::{PackageInfoV2, RelationOrSoname};
use alpm_db::desc::DbDescFileV2;
use alpm_db::desc::Section::Version;
use tar::Archive;
use tracing::{debug, error, warn};
use xz2::read::XzDecoder;
use zstd::stream::read::Decoder as ZstdDecoder;

pub fn repo_add_impl(
    pkgfile: &str,
    db_archive: String,
    files_archive: String,
) -> anyhow::Result<()> {
    let mut files = vec![];

    // Path to the .tar.zst file
    let file = File::open(Path::new(pkgfile))?;
    let ext = Path::new(pkgfile).extension().and_then(|e| e.to_str());

    // Select the appropriate decompression method
    let decompressor: Box<dyn Read> = match ext {
        Some("zst") => {
            let decoder = ZstdDecoder::new(BufReader::new(file))?;
            Box::new(decoder)
        }
        Some("xz") => {
            let decoder = XzDecoder::new(BufReader::new(file));
            Box::new(decoder)
        }
        _ => {
            bail!("Unsupported file type");
        }
    };
    let mut archive = Archive::new(decompressor);

    let mut pkginfo = None;

    // Iterate over the entries in the tar archive
    for entry in archive.entries()? {
        match entry {
            Ok(entry) => {
                if let Ok(path) = entry.path() {
                    if !path.display().to_string().starts_with('.') {
                        files.push(format!("{}", path.display()));
                    }

                    if path == Path::new(".PKGINFO") {
                        debug!("Found .PKGINFO file in '{pkgfile}'.");

                        let mut reader = io::BufReader::new(entry);
                        let mut pkginfo_str = String::new();
                        reader.read_to_string(&mut pkginfo_str)?;

                        pkginfo = Some(PackageInfoV2::from_str(pkginfo_str.as_str())?);
                    }
                }
            }
            Err(e) => warn!("Error reading entry: {e:?}"),
        }
    }

    let pkginfo = match pkginfo {
        None => {
            error!("No valid .PKGINFO found in '{pkgfile}'.");
            bail!("No valid .PKGINFO found in '{pkgfile}'.");
        }
        Some(v) => v,
    };

    debug!("Calculating compressed size for '{pkgfile}'.");
    let csize = fs::metadata(pkgfile)?.len() as usize;

    debug!("Calculating checksums for '{pkgfile}'.");
    let (md5sum, sha256sum) = calc_checksums(pkgfile)?;

    let filename = Path::new(pkgfile)
        .file_name()
        .ok_or(anyhow!("invalid path"))?
        .to_str()
        .ok_or(anyhow!("invalid path"))?
        .to_string();

    let dir_name = format!("{}-{}", pkginfo.pkgname, pkginfo.pkgver);

    debug!("Creating DESC file for db entry");
    let desc = DbDescFileV2{
        name: pkginfo.pkgname,
        version: pkginfo.pkgver.into(),
        base: pkginfo.pkgbase,
        description: Some(pkginfo.pkgdesc),
        url: Some(pkginfo.url),
        arch: pkginfo.arch,
        builddate: pkginfo.builddate,
        installdate: 0,
        packager: pkginfo.packager,
        size: pkginfo.size,
        groups: pkginfo.group,
        reason: None,
        license: pkginfo.license,
        validation: vec![],
        replaces: pkginfo.replaces.iter().map(|x| {x.name.clone()}).collect(),
        depends: pkginfo.depend.iter()
            .filter_map(|x| {
                match x {
                    RelationOrSoname::Relation(v) => Some(v.clone()),
                    _ => None,
                }
            })
            .collect(),
        optdepends: pkginfo.optdepend,
        conflicts: pkginfo.conflict.iter().map(|x| {x.name.clone()}).collect(),
        provides: pkginfo.provides.iter()
            .filter_map(|x| {
                match x {
                    RelationOrSoname::Relation(v) => Some(v.name.clone()),
                    _ => None,
                }
            })
            .collect(),
        xdata: pkginfo.xdata,
    };

    //let mut desc = Desc::from(pkginfo);
    //desc.filename = filename.clone();
    //desc.md5sum = md5sum.clone();
    //desc.csize = csize.to_string();
    //desc.sha256sum = sha256sum.clone();
    // todo the desc implementation of alpm doesn't contain the isize, csize and sha256 fields and so on
    let desc_str = desc.to_string();

    debug!("Adding DESC and FILES entries to db archive");
    add_to_db_file(
        desc_str.clone(),
        dir_name.clone(),
        "desc".to_string(),
        db_archive,
    )?;

    files.sort();
    let files_comb = format!("%FILES%\n{}", files.join("\n"));
    add_to_db_file(
        desc_str,
        dir_name.clone(),
        "desc".to_string(),
        files_archive.clone(),
    )?;
    add_to_db_file(files_comb, dir_name, "files".to_string(), files_archive)?;

    Ok(())
}

fn calc_checksums(path: &str) -> anyhow::Result<(String, String)> {
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let md5sum = format!("{:x}", md5::compute(&buffer));
    let mut hasher = Sha256::new();
    hasher.update(&buffer);
    let sha256sum = format!("{:x}", hasher.finalize());

    Ok((md5sum, sha256sum))
}
