use crate::repo_database::db::remove_from_db_file;

pub fn repo_remove_impl(
    filename: String,
    db_archive: String,
    files_archive: String,
) -> anyhow::Result<()> {
    let (dir_name, _) = split_last_occurrence(filename.as_str(), '-');
    remove_from_db_file(db_archive, dir_name.to_string())?;
    remove_from_db_file(files_archive, dir_name.to_string())?;
    Ok(())
}

fn split_last_occurrence(s: &str, delimiter: char) -> (&str, &str) {
    match s.rfind(delimiter) {
        Some(pos) => (&s[..pos], &s[pos + delimiter.len_utf8()..]),
        None => (s, ""),
    }
}
