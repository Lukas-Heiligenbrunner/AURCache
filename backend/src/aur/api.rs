use anyhow::anyhow;
use aur_rs::{Package, Request};

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
