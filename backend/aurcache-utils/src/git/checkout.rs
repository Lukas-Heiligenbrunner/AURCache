use anyhow::anyhow;
use git2::Repository;
use std::path::PathBuf;

/// checkout git repo at specific ref
/// parts of this are not 'Send' so they need to be scoped
pub fn checkout_repo_ref(
    git_repo: String,
    git_ref: String,
    path: PathBuf,
) -> anyhow::Result<Repository> {
    // checkout repo
    let repo = Repository::clone(git_repo.as_str(), &path)?;

    {
        // Resolve the ref to an object
        let (object, reference) = repo.revparse_ext(git_ref.as_str())?;

        // Checkout the tree (updates working directory)
        repo.checkout_tree(&object, None)?;

        // If it's a branch or tag, make HEAD point to it
        if let Some(reference) = reference {
            repo.set_head(reference.name().ok_or(anyhow!("Reference name invalid"))?)?;
        } else {
            // Detached HEAD for a commit hash
            repo.set_head_detached(object.id())?;
        }
    }
    Ok(repo)
}
