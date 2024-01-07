use crate::aur::pkgbuild::{parse, PkgBuild};
use crate::pkgbuild;
use anyhow::anyhow;
use aur_rs::{Package, Request};
use flate2::bufread::GzDecoder;
use rocket::http::hyper::body::HttpBody;
use sea_orm::ColIdx;
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

pub async fn download_pkgbuild(url: &str, dest_dir: &str) -> anyhow::Result<(String, PkgBuild)> {
    let file_data = match download_file(url).await {
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

    let (basepath, pkgbuild) = unpack_tar_gz(&file_data, dest_dir)?;
    Ok((basepath, pkgbuild))
}

async fn download_file(url: &str) -> anyhow::Result<Vec<u8>> {
    let response = reqwest::get(url).await?;

    // extract name of file without extension
    // todo might be also possible here to use package name
    // let t = response
    //     .url()
    //     .path_segments()
    //     .and_then(|segments| segments.last())
    //     .ok_or(anyhow!("no segments"))?
    //     .split('.')
    //     .collect::<Vec<&str>>()
    //     .first()
    //     .ok_or(anyhow!(""))?
    //     .to_string();
    //
    // println!("{}", t);

    let r = response.bytes().await?;
    Ok(r.to_vec())
}

fn unpack_tar_gz(data: &[u8], target_dir: &str) -> anyhow::Result<(String, PkgBuild)> {
    let tar = GzDecoder::new(data);
    let mut archive = Archive::new(tar);

    let mut basePath = None;
    let mut pkgbuild = None;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let entry_path = Path::new(target_dir).join(path.clone()).clone();
        let p = entry_path.to_str().to_owned().unwrap().to_owned();

        entry.unpack(entry_path)?;

        if p.ends_with("PKGBUILD") {
            // get package basepath directory -> might differ from pkgname
            basePath = Some(
                Path::new(p.as_str())
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_str()
                    .ok_or(anyhow!("couldn't convert path to string"))?.to_string(),
            );

            let pkgbuild_str = fs::read_to_string(p)?;
            let pkgbuildd = parse(pkgbuild_str)?;
            pkgbuild = Some(pkgbuildd)
        }
    }

    let basePath = basePath.ok_or(anyhow!("No buildpkg parent found"))?;
    let pkgbuild = pkgbuild.ok_or(anyhow!("Failed to build pkgbuild file"))?;
    Ok((basePath, pkgbuild))
}
