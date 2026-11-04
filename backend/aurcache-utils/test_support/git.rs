//! Git fixture mechanics shared by unit and integration tests.
use std::path::Path;

pub fn create_repo(path: &Path, files: &[(&str, &str)]) {
    let repo = git2::Repository::init(path).unwrap();
    let mut index = repo.index().unwrap();
    for (name, content) in files {
        std::fs::write(path.join(name), content).unwrap();
        index.add_path(Path::new(name)).unwrap();
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
}
