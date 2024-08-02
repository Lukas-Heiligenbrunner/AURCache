use anyhow::anyhow;

use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use tar::Archive;
use zstd::Decoder;

use crate::pkginfo::parser::Pkginfo;
use crate::repo_database::db::add_to_db_file;
use crate::repo_database::desc::Desc;

pub fn repo_add_impl(
    pkgfile: &str,
    db_archive: String,
    files_archive: String,
) -> anyhow::Result<()> {
    let mut files = vec![];

    // Path to the .tar.zst file
    let file = File::open(Path::new(pkgfile))?;
    let decoder = Decoder::new(file)?;
    let mut archive = Archive::new(decoder);

    let mut pkginfo = Pkginfo::new();

    // Iterate over the entries in the tar archive
    for entry in archive.entries()? {
        let entry = entry?;

        if !entry.path()?.display().to_string().starts_with('.') {
            files.push(format!("{}", entry.path()?.display()));
        }

        if entry.path()? == Path::new(".PKGINFO") {
            pkginfo.parse(entry)?;
        }
    }

    // Ensure $pkgname and $pkgver variables were found
    if pkginfo.pkgname.is_empty() || pkginfo.pkgver.is_empty() {
        eprintln!("Invalid package file '{}'.", pkgfile);
        return Err(anyhow!("Invalid package file"));
    }

    // Compute base64'd PGP signature
    pkginfo.set_signature(pkgfile)?;

    let csize = fs::metadata(pkgfile)?.len() as usize;

    // Compute checksums
    let (md5sum, sha256sum) = calc_checksums(pkgfile)?;

    let filename = Path::new(pkgfile)
        .file_name()
        .ok_or(anyhow!("invalid path"))?
        .to_str()
        .ok_or(anyhow!("invalid path"))?
        .to_string();

    let dir_name = format!("{}-{}", pkginfo.pkgname, pkginfo.pkgver);

    let mut desc = Desc::from(pkginfo);
    desc.filename = filename.clone();
    desc.md5sum = md5sum.clone();
    desc.csize = csize.to_string();
    desc.sha256sum = sha256sum.clone();
    let desc_str = desc.to_string();

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
