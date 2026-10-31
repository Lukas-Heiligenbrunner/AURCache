use anyhow::anyhow;
use aurcache_deps::AurClient;

/// The whitespace-separated terms of a query, lowercased.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect()
}

/// Whether an AUR package matches every term in `terms`, the way the AUR's
/// `by=name-desc` search does per term: each one has to be a
/// case-insensitive substring of either the name or the description.
fn package_matches_terms(name: &str, description: Option<&str>, terms: &[String]) -> bool {
    let name = name.to_lowercase();
    let description = description.unwrap_or("").to_lowercase();
    terms
        .iter()
        .all(|term| name.contains(term) || description.contains(term))
}

/// Query the AUR for packages matching the given query string.
///
/// A multi-word query asks for packages matching *every* word: `gnome system
/// monitor` means `gnome` and `system` and `monitor`, because no package name
/// contains the phrase with its spaces. Only one request goes out — for the
/// longest term, which is the most selective — and the remaining terms narrow
/// the answer locally. Anything matching all terms necessarily matches that
/// one, so the narrowed set is the whole answer, not a sample of it.
pub async fn query_aur(
    client: &AurClient,
    query: &str,
) -> anyhow::Result<Vec<aurcache_deps::Package>> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let base = terms
        .iter()
        .max_by_key(|term| term.len())
        .expect("terms is not empty");
    let mut results = client
        .search_by_name(base)
        .await
        .map_err(|e| anyhow!("failed to query AUR: {e}"))?;
    results.retain(|pkg| package_matches_terms(&pkg.name, pkg.description.as_deref(), &terms));
    results.sort_by(|a, b| b.popularity.total_cmp(&a.popularity));
    Ok(results)
}

/// Retrieve AUR package information by its name.
/// Returns `None` if the package is not found.
pub async fn get_package_info(
    client: &AurClient,
    pkg_name: &str,
) -> anyhow::Result<Option<aurcache_deps::Package>> {
    client
        .info_of(pkg_name)
        .await
        .map_err(|e| anyhow!("failed to get package info: {e}"))
}

#[cfg(test)]
mod tests {
    use super::package_matches_terms;

    fn matches(name: &str, description: Option<&str>, query_terms: &[&str]) -> bool {
        package_matches_terms(
            name,
            description,
            &query_terms
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn every_word_has_to_match_somewhere() {
        assert!(matches(
            "gnome-shell-extension-system-monitor-next-git",
            None,
            &["gnome", "system", "monitor"],
        ));
        assert!(!matches("gnome-shell", None, &["gnome", "system"]));
    }

    #[test]
    fn a_word_may_match_the_description_instead_of_the_name() {
        assert!(matches(
            "alacritty",
            Some("A fast terminal emulator"),
            &["alacritty", "terminal"],
        ));
        assert!(!matches(
            "alacritty",
            Some("A fast terminal emulator"),
            &["alacritty", "browser"],
        ));
    }

    #[test]
    fn matching_ignores_case() {
        assert!(matches("Hello", None, &["hello"]));
    }
}
