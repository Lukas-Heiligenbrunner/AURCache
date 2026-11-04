#[tokio::test]
#[ignore = "hits the real AUR"]
async fn a_query_larger_than_the_url_limit_succeeds() {
    let client = aurcache_deps::AurClient::new();
    // Real packages, padded with names that do not exist, so the query is far
    // past the ~300-package ceiling a single request has.
    let mut names: Vec<String> = vec!["hello".into(), "yay".into(), "paru".into()];
    names.extend((0..600).map(|i| format!("definitely-not-a-real-package-{i}")));
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();

    let found = client
        .multi_info_of(&refs)
        .await
        .expect("chunked query should succeed");
    let found_names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
    assert!(found_names.contains(&"hello"), "{found_names:?}");
    assert!(found_names.contains(&"yay"), "{found_names:?}");
}
