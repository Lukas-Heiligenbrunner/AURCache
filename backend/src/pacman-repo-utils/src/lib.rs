use anyhow::anyhow;
use base64;
use base64::{engine::general_purpose, Engine as _};
use md5;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, BufRead, Cursor, Read, Write};
use std::path::Path;
use tar::{Archive, Builder, Header};
use zstd::Decoder;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

pub fn repo_add(pkgfile: &str, db_archive: String, files_archive: String) -> anyhow::Result<()> {
    // Initialize variables
    let mut _groups = vec![];
    let mut _licenses = vec![];
    let mut _replaces = vec![];
    let mut _depends = vec![];
    let mut _conflicts = vec![];
    let mut _provides = vec![];
    let mut _optdepends = vec![];
    let mut _makedepends = vec![];
    let mut _checkdepends = vec![];
    let mut pkgname = String::new();
    let mut pkgbase = String::new();
    let mut pkgver = String::new();
    let mut pkgdesc = String::new();
    let mut size = 0;
    let mut url = String::new();
    let mut arch = String::new();
    let mut builddate = String::new();
    let mut packager = String::new();
    let mut pgpsig = String::new();

    let mut files = vec![];

    // Path to the .tar.zst file
    let file = File::open(Path::new(pkgfile))?;
    let decoder = Decoder::new(file)?;
    let mut archive = Archive::new(decoder);

    // Iterate over the entries in the tar archive
    for entry in archive.entries()? {
        let entry = entry?;

        if !entry.path()?.display().to_string().starts_with(".") {
            files.push(format!("{}", entry.path()?.display()));
        }

        if entry.path()? == Path::new(".PKGINFO") {
            let reader = io::BufReader::new(entry);
            for line in reader.lines() {
                let line = line?;
                if line.starts_with('#') {
                    continue;
                }
                let (key, value) = match line.split_once('=') {
                    None => continue,
                    Some((key, value)) => (key.trim(), value.trim()),
                };
                match key {
                    "group" => _groups.push(value.to_string()),
                    "license" => _licenses.push(value.to_string()),
                    "replaces" => _replaces.push(value.to_string()),
                    "depend" => _depends.push(value.to_string()),
                    "conflict" => _conflicts.push(value.to_string()),
                    "provides" => _provides.push(value.to_string()),
                    "optdepend" => _optdepends.push(value.to_string()),
                    "makedepend" => _makedepends.push(value.to_string()),
                    "checkdepend" => _checkdepends.push(value.to_string()),
                    "pkgname" => pkgname = value.to_string(),
                    "pkgbase" => pkgbase = value.to_string(),
                    "pkgver" => pkgver = value.to_string(),
                    "pkgdesc" => pkgdesc = value.to_string(),
                    "size" => size = value.parse().unwrap_or(0),
                    "url" => url = value.to_string(),
                    "arch" => arch = value.to_string(),
                    "builddate" => builddate = value.to_string(),
                    "packager" => packager = value.to_string(),
                    _ => {}
                }
            }
        }
    }

    // Ensure $pkgname and $pkgver variables were found
    if pkgname.is_empty() || pkgver.is_empty() {
        eprintln!("Invalid package file '{}'.", pkgfile);
        return Err(anyhow!("Invalid package file"));
    }

    // Compute base64'd PGP signature
    let sigfile = format!("{}.sig", pkgfile);
    if Path::new(&sigfile).exists() {
        let sigdata = fs::read(&sigfile)?;
        if sigdata.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
            eprintln!("Cannot use armored signatures for packages: {}", sigfile);
            return Err(anyhow!("Invalid package signature file"));
        }
        let pgpsigsize = sigdata.len();
        if pgpsigsize > 16384 {
            eprintln!("Invalid package signature file '{}'.", sigfile);
            return Err(anyhow!("Invalid package signature file"));
        }

        pgpsig = general_purpose::STANDARD.encode(&sigdata);
    }

    let csize = fs::metadata(pkgfile)?.len() as usize;

    // Compute checksums
    let mut file = File::open(pkgfile)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let md5sum = format!("{:x}", md5::compute(&buffer));
    let mut hasher = Sha256::new();
    hasher.update(&buffer);
    let sha256sum = format!("{:x}", hasher.finalize());

    let filename = Path::new(pkgfile)
        .file_name()
        .ok_or(anyhow!("invalid path"))?
        .to_str()
        .ok_or(anyhow!("invalid path"))?
        .to_string();

    // Create desc file
    let mut desc_lines = vec![];
    desc_lines.push(add_desc_entry("FILENAME", vec![filename]));
    desc_lines.push(add_desc_entry("NAME", vec![pkgname.clone()]));
    desc_lines.push(add_desc_entry("BASE", vec![pkgbase]));
    desc_lines.push(add_desc_entry("VERSION", vec![pkgver.clone()]));
    desc_lines.push(add_desc_entry("DESC", vec![pkgdesc]));
    desc_lines.push(add_desc_entry("GROUPS", _groups));
    desc_lines.push(add_desc_entry("CSIZE", vec![csize.to_string()]));
    desc_lines.push(add_desc_entry("ISIZE", vec![size.to_string()]));
    desc_lines.push(add_desc_entry("MD5SUM", vec![md5sum]));
    desc_lines.push(add_desc_entry("SHA256SUM", vec![sha256sum]));
    desc_lines.push(add_desc_entry("PGPSIG", vec![pgpsig]));
    desc_lines.push(add_desc_entry("URL", vec![url]));
    desc_lines.push(add_desc_entry("LICENSE", _licenses));
    desc_lines.push(add_desc_entry("ARCH", vec![arch]));
    desc_lines.push(add_desc_entry("BUILDDATE", vec![builddate]));
    desc_lines.push(add_desc_entry("PACKAGER", vec![packager]));
    desc_lines.push(add_desc_entry("REPLACES", _replaces));
    desc_lines.push(add_desc_entry("CONFLICTS", _conflicts));
    desc_lines.push(add_desc_entry("PROVIDES", _provides));
    desc_lines.push(add_desc_entry("DEPENDS", _depends));
    desc_lines.push(add_desc_entry("OPTDEPENDS", _optdepends));
    desc_lines.push(add_desc_entry("MAKEDEPENDS", _makedepends));
    desc_lines.push(add_desc_entry("CHECKDEPENDS", _checkdepends));
    let desc = desc_lines.join("");

    let dir_name = format!("{}-{}", pkgname, pkgver);
    add_to_db_file(
        desc.clone(),
        dir_name.clone(),
        "desc".to_string(),
        db_archive,
    )?;

    files.sort();
    let files_comb = format!("%FILES%\n{}", files.join("\n"));
    add_to_db_file(
        desc,
        dir_name.clone(),
        "desc".to_string(),
        files_archive.clone(),
    )?;
    add_to_db_file(files_comb, dir_name, "files".to_string(), files_archive)?;

    Ok(())
}

fn add_desc_entry(header: &str, values: Vec<String>) -> String {
    if values.is_empty() {
        return String::new();
    }
    if values.len() == 1 && values.get(0).unwrap().eq("") {
        return String::new();
    }
    format!("%{}%\n{}\n\n", header, values.join("\n"))
}

fn split_last_occurrence(s: &str, delimiter: char) -> (&str, &str) {
    match s.rfind(delimiter) {
        Some(pos) => (&s[..pos], &s[pos + delimiter.len_utf8()..]),
        None => (s, ""),
    }
}

pub fn repo_remove(
    filename: String,
    db_archive: String,
    files_archive: String,
) -> anyhow::Result<()> {
    let (dir_name, _) = split_last_occurrence(filename.as_str(), '-');
    remove_from_db_file(db_archive, dir_name.to_string())?;
    remove_from_db_file(files_archive, dir_name.to_string())?;
    Ok(())
}

fn remove_from_db_file(db_archive: String, dir_name: String) -> anyhow::Result<()> {
    if !Path::new(&db_archive).exists() {
        return Ok(());
    }

    let mut new_archive_data = Vec::new();
    {
        let mut existing_archive_data = Vec::new();
        {
            let mut file = File::open(db_archive.clone())?;
            file.read_to_end(&mut existing_archive_data)?;
        }
        // Decode the existing archive
        let mut archive = Archive::new(GzDecoder::new(Cursor::new(existing_archive_data)));

        let enc = GzEncoder::new(&mut new_archive_data, Compression::default());
        let mut tar_builder = Builder::new(enc);

        // Copy existing entries to the new archive
        for entry in archive.entries()? {
            let mut entry = entry?;

            // skip file and folder we want to delete
            if entry.header().path()?.starts_with(dir_name.clone()) {
                continue;
            }

            tar_builder.append(&entry.header().clone(), &mut entry)?;
        }

        tar_builder.finish()?;
    }

    let mut new_file = File::create(db_archive)?;
    new_file.write_all(&new_archive_data)?;
    Ok(())
}

fn add_to_db_file(
    content: String,
    dir_name: String,
    file_name: String,
    db_archive: String,
) -> anyhow::Result<()> {
    // Check if the archive exists
    let archive_exists = Path::new(&db_archive).exists();

    let mut new_archive_data = Vec::new();
    {
        let mut builder = if archive_exists {
            let mut existing_archive_data = Vec::new();
            {
                let mut file = File::open(db_archive.clone())?;
                file.read_to_end(&mut existing_archive_data)?;
            }
            // Decode the existing archive
            let mut archive = Archive::new(GzDecoder::new(Cursor::new(existing_archive_data)));

            let enc = GzEncoder::new(&mut new_archive_data, Compression::default());
            let mut tar_builder = Builder::new(enc);

            // Copy existing entries to the new archive
            for entry in archive.entries()? {
                let mut entry = entry?;
                tar_builder.append(&entry.header().clone(), &mut entry)?;
            }
            tar_builder
        } else {
            // Create a new archive
            let encoder = GzEncoder::new(&mut new_archive_data, Compression::default());
            let builder = Builder::new(encoder);
            builder
        };

        // Add a folder
        let mut header = Header::new_gnu();
        header.set_path(dir_name.clone())?;
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        builder.append(&header, io::empty())?;

        // Add a file
        let mut header = Header::new_gnu();
        header.set_path(format!("{}/{}", dir_name, file_name))?;
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, content.as_bytes())?;

        builder.finish()?;
    }
    let mut new_file = File::create(db_archive)?;
    new_file.write_all(&new_archive_data)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repoadd() {
        repo_add(
            "/home/lukas/RustroverProjects/untitled/backend/repo/yay-12.3.5-1-x86_64.pkg.tar.zst",
            "test.db.tar.gz".to_string(),
            "test.files.tar.gz".to_string(),
        )
        .unwrap()
    }

    #[test]
    fn repodel() {
        repo_remove(
            "yay-12.3.5-1".to_string(),
            "test.db.tar.gz".to_string(),
            "test.files.tar.gz".to_string(),
        )
        .unwrap()
    }
}
