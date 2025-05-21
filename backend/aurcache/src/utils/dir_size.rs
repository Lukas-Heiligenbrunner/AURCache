use std::fs;
use std::path::PathBuf;

pub fn dir_size(path: impl Into<PathBuf>) -> anyhow::Result<u64> {
    fn dir_size(mut dir: fs::ReadDir) -> anyhow::Result<u64> {
        dir.try_fold(0, |acc, file| {
            let file = file?;
            let size = match file.metadata()? {
                data if data.is_dir() => dir_size(fs::read_dir(file.path())?)?,
                data => data.len(),
            };
            Ok(acc + size)
        })
    }

    dir_size(fs::read_dir(path.into())?)
}
