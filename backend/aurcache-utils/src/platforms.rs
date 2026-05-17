pub fn split_platform_list(platforms: &str) -> Vec<&str> {
    platforms
        .split(';')
        .map(str::trim)
        .filter(|platform| !platform.is_empty())
        .collect()
}

pub fn platform_list_contains(platforms: &str, target: &str) -> bool {
    if platforms.trim().is_empty() {
        return true;
    }

    split_platform_list(platforms)
        .into_iter()
        .any(|platform| platform == target)
}

#[cfg(test)]
mod tests {
    use super::{platform_list_contains, split_platform_list};

    #[test]
    fn empty_platform_list_matches_any_platform() {
        assert!(platform_list_contains("", "x86_64"));
    }

    #[test]
    fn semicolon_delimited_platform_list_matches_exact_platform() {
        assert!(platform_list_contains("x86_64;aarch64", "aarch64"));
        assert!(!platform_list_contains("x86_64;aarch64", "armv7h"));
    }

    #[test]
    fn split_platform_list_trims_and_skips_empty_entries() {
        assert_eq!(
            split_platform_list(" x86_64 ; ; aarch64 "),
            vec!["x86_64", "aarch64"]
        );
    }
}
