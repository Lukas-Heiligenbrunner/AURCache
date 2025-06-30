use anyhow::anyhow;
use aur_rs::{Package, Request};
use backon::{FibonacciBuilder, Retryable};
use std::time::Duration;

// https://wiki.archlinux.org/title/Aurweb_RPC_interface
// API rate limit 4000 requests per day

/// Query the AUR for packages matching the given query string
pub async fn query_aur(query: &str) -> anyhow::Result<Vec<Package>> {
    let request = Request::default();
    let response = (|| async { request.search_package_by_name(query).await })
        .retry(
            FibonacciBuilder::default()
                .with_min_delay(Duration::from_millis(500))
                .with_max_times(4),
        )
        .await
        .map_err(|e| anyhow!("failed to get package: {}", e))?;

    let mut response = response.results;
    response.sort_by(|x, x1| x.popularity.partial_cmp(&x1.popularity).unwrap().reverse());

    Ok(response)
}

/// Retrieve AUR package information by its name.
pub async fn get_package_info(pkg_name: &str) -> anyhow::Result<Package> {
    let request = Request::default();
    let mut response = (|| async { request.search_info_by_name(pkg_name).await })
        .retry(
            FibonacciBuilder::default()
                .with_min_delay(Duration::from_millis(500))
                .with_max_times(4),
        )
        .await
        .map_err(|e| anyhow!("failed to get package: {}", e))?;

    response.results.pop().ok_or(anyhow!("no package found"))
}
