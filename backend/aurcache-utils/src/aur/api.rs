use anyhow::anyhow;
use aurcache_deps::AurClient;
use std::sync::OnceLock;

fn client() -> &'static AurClient {
    static CLIENT: OnceLock<AurClient> = OnceLock::new();
    CLIENT.get_or_init(AurClient::new)
}

/// Query the AUR for packages matching the given query string.
pub async fn query_aur(query: &str) -> anyhow::Result<Vec<aurcache_deps::Package>> {
    let mut results = client()
        .search_by_name(query)
        .await
        .map_err(|e| anyhow!("failed to query AUR: {e}"))?;
    results.sort_by(|x, x1| x.popularity.partial_cmp(&x1.popularity).unwrap().reverse());
    Ok(results)
}

/// Retrieve AUR package information by its name.
/// Returns `None` if the package is not found.
pub async fn get_package_info(pkg_name: &str) -> anyhow::Result<Option<aurcache_deps::Package>> {
    client()
        .info_of(pkg_name)
        .await
        .map_err(|e| anyhow!("failed to get package info: {e}"))
}
