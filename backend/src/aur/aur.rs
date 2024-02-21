use anyhow::anyhow;
use aur_rs::{Package, Request};
use flate2::bufread::GzDecoder;
use std::fs;
use std::path::Path;
use tar::Archive;

pub async fn query_aur(query: &str) -> anyhow::Result<Vec<Package>> {
    let request = Request::default();
    let response = request.search_package_by_name(query).await;

    let response = match response {
        Ok(v) => v,
        Err(_) => {
            return Err(anyhow!("failed to search"));
        }
    };

    let mut response = response.results;
    response.sort_by(|x, x1| x.popularity.partial_cmp(&x1.popularity).unwrap().reverse());

    Ok(response)
}

pub async fn get_info_by_name(pkg_name: &str) -> anyhow::Result<Package> {
    let request = Request::default();
    let response = request.search_info_by_name(pkg_name).await;

    let mut response = match response {
        Ok(v) => v,
        Err(_) => {
            return Err(anyhow!("failed to get package"));
        }
    };

    let response = match response.results.pop() {
        None => {
            return Err(anyhow!("no package found"));
        }
        Some(v) => v,
    };

    Ok(response)
}

pub async fn download_pkgbuild(url: &str, dest_dir: &str) -> anyhow::Result<String> {
    let (file_data, file_name) = match download_file(url).await {
        Ok(data) => data,
        Err(e) => {
            return Err(anyhow!("Error downloading file: {}", e));
        }
    };

    // Check if the directory exists
    if fs::metadata(dest_dir).is_err() {
        // Create the directory if it does not exist
        fs::create_dir(dest_dir)?;
    }

    unpack_tar_gz(&file_data, dest_dir)?;
    Ok(file_name)
}

async fn download_file(url: &str) -> anyhow::Result<(Vec<u8>, String)> {
    let response = reqwest::get(url).await?;

    // extract name of file without extension
    // todo might be also possible here to use package name
    let t = response
        .url()
        .path_segments()
        .and_then(|segments| segments.last())
        .ok_or(anyhow!("no segments"))?
        .split('.')
        .collect::<Vec<&str>>()
        .first()
        .ok_or(anyhow!(""))?
        .to_string();

    println!("{}", t);

    let r = response.bytes().await?;
    Ok((r.to_vec(), t))
}

fn unpack_tar_gz(data: &[u8], target_dir: &str) -> anyhow::Result<()> {
    let tar = GzDecoder::new(data);
    let mut archive = Archive::new(tar);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let entry_path = Path::new(target_dir).join(path);

        entry.unpack(entry_path)?;
    }

    Ok(())
}
