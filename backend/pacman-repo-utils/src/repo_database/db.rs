use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::path::Path;
use tar::{Archive, Builder, Header};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

pub fn remove_from_db_file(db_archive: String, dir_name: String) -> anyhow::Result<()> {
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
        for mut entry in (archive.entries()?).flatten() {
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

pub fn add_to_db_file(
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
            for mut entry in archive.entries()?.flatten() {
                tar_builder.append(&entry.header().clone(), &mut entry)?;
            }
            tar_builder
        } else {
            // Create a new archive
            let encoder = GzEncoder::new(&mut new_archive_data, Compression::default());

            Builder::new(encoder)
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
