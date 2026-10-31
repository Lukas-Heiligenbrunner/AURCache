//! Package metadata read from the source checkout rather than the AUR.
//!
//! The AUR RPC used to supply this, which cost a network round trip on a quota
//! of 4000 calls a day. Everything here is already on disk: the version-check
//! scheduler clones every package's repository on every pass, for every source
//! type, to resolve VCS sources.
//!
//! Reading it locally also fixes two things the RPC could not:
//!
//! - **Git-sourced packages get metadata at all.** They have no AUR entry, so
//!   their page showed no description, licenses or maintainer.
//! - **It reflects the patched PKGBUILD**, which is what actually gets built,
//!   rather than the AUR's copy of it.

use alpm_srcinfo::SourceInfoV1;

/// What a checkout can tell us about a package, beyond its dependencies.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SourceMetadata {
    pub description: Option<String>,
    pub project_url: Option<String>,
    /// Joined with ", " for display; `.SRCINFO` may list several.
    pub licenses: Option<String>,
    pub maintainer: Option<String>,
    /// Unix seconds of the repository's first commit — when the package was
    /// first submitted.
    pub first_submitted: Option<i64>,
    /// Unix seconds of the repository's newest commit — when the packaging was
    /// last touched. Not the same as the upstream project's activity.
    pub last_modified: Option<i64>,
}

/// The parts that come out of a parsed `.SRCINFO`.
#[must_use]
pub fn from_sourceinfo(sourceinfo: &SourceInfoV1) -> SourceMetadata {
    use alpm_srcinfo::source_info::v1::package::Override;

    let base = &sourceinfo.base;

    // Split packages sometimes only describe the sub-packages: `backintime`
    // has no top-level `pkgdesc`, but its `backintime` sub-package does. Fall
    // back to the first sub-package description so those packages still get
    // one.
    let description = base
        .description
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| {
            sourceinfo
                .packages
                .iter()
                .find_map(|pkg| match &pkg.description {
                    Override::Yes { value } => Some(value.to_string()),
                    _ => None,
                })
        });

    SourceMetadata {
        description,
        project_url: base.url.as_ref().map(ToString::to_string),
        licenses: (!base.licenses.is_empty()).then(|| {
            base.licenses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }),
        maintainer: None,
        first_submitted: None,
        last_modified: None,
    }
}

/// The maintainer named in a PKGBUILD's leading comments.
///
/// A convention rather than a format: `makepkg --printsrcinfo` drops comments,
/// so `.SRCINFO` has no maintainer field and this is the only place the name
/// exists. Real PKGBUILDs are inconsistent about it — `hello` in the AUR has
/// `# Maintainer: Matthew Sexton <mssxtn@gmail.com` with no closing bracket —
/// so this takes whatever follows the colon rather than trying to parse a
/// name/email pair.
///
/// `Contributor` is deliberately ignored: it names past authors, not the
/// person currently responsible.
#[must_use]
pub fn maintainer_from_pkgbuild(pkgbuild: &str) -> Option<String> {
    pkgbuild
        .lines()
        // Only the header block. A `# Maintainer:` mentioned inside a function
        // body is talking about something else.
        .take_while(|line| {
            let line = line.trim_start();
            line.is_empty() || line.starts_with('#')
        })
        .find_map(|line| {
            let rest = line.trim_start().trim_start_matches('#').trim_start();
            let (label, value) = rest.split_once(':')?;
            label
                .trim()
                .eq_ignore_ascii_case("maintainer")
                .then(|| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::maintainer_from_pkgbuild;

    /// The spellings that actually appear in the AUR, including `hello`'s
    /// unclosed bracket.
    #[test]
    fn the_maintainer_comment_is_read_however_it_is_written() {
        for (pkgbuild, expected) in [
            (
                "# Maintainer: Matthew Sexton <mssxtn@gmail.com\npkgname=hello",
                "Matthew Sexton <mssxtn@gmail.com",
            ),
            ("#Maintainer:someone\npkgname=x", "someone"),
            ("#  maintainer :  Lower Case <a@b>\n", "Lower Case <a@b>"),
            ("# MAINTAINER: Shouty <s@t>\n", "Shouty <s@t>"),
        ] {
            assert_eq!(
                maintainer_from_pkgbuild(pkgbuild).as_deref(),
                Some(expected),
                "{pkgbuild:?}"
            );
        }
    }

    /// `Contributor` names previous authors, not the person responsible now.
    #[test]
    fn contributors_are_not_maintainers() {
        let pkgbuild = "\
#Contributor: Michał Wojdyła < micwoj9292 at gmail dot com >
# Contributor: leo <leotemplin@yahoo.de>
pkgname=hello
";
        assert_eq!(maintainer_from_pkgbuild(pkgbuild), None);
    }

    /// The first maintainer wins where several are listed.
    #[test]
    fn the_first_maintainer_is_used() {
        let pkgbuild = "# Maintainer: First <a@b>\n# Maintainer: Second <c@d>\npkgname=x";
        assert_eq!(
            maintainer_from_pkgbuild(pkgbuild).as_deref(),
            Some("First <a@b>")
        );
    }

    /// Only the header block counts, so a mention further down — in a comment
    /// inside a function, say — is not mistaken for the real thing.
    #[test]
    fn only_the_leading_comment_block_is_searched() {
        let pkgbuild = "\
pkgname=demo
build() {
  # Maintainer: not-a-real-maintainer
  make
}
";
        assert_eq!(maintainer_from_pkgbuild(pkgbuild), None);
    }

    /// A package with no maintainer comment has none, rather than an empty
    /// string that would render as a blank field.
    #[test]
    fn a_pkgbuild_without_the_comment_has_no_maintainer() {
        assert_eq!(maintainer_from_pkgbuild("pkgname=demo\n"), None);
        assert_eq!(maintainer_from_pkgbuild("# Maintainer:\npkgname=x"), None);
    }

    use super::from_sourceinfo;
    use alpm_srcinfo::SourceInfoV1;

    fn metadata_description(srcinfo: &str) -> Option<String> {
        let parsed = SourceInfoV1::from_string(srcinfo).expect("fixture parses");
        from_sourceinfo(&parsed).description
    }

    /// Split packages like `backintime` carry `pkgdesc` only on the
    /// sub-packages, so the first sub-package description is used.
    #[test]
    fn a_missing_top_level_description_falls_back_to_the_first_subpackage() {
        let srcinfo = "pkgbase = backintime\n\tpkgver = 1.4.3\n\tpkgrel = 1\n\tarch = any\n\npkgname = backintime\n\tpkgdesc = Back In Time description\n\npkgname = backintime-cli\n";
        assert_eq!(
            metadata_description(srcinfo).as_deref(),
            Some("Back In Time description")
        );
    }

    /// The top-level description still wins when it exists.
    #[test]
    fn a_top_level_description_is_not_overridden_by_subpackages() {
        let srcinfo = "pkgbase = demo\n\tpkgdesc = Top level\n\tpkgver = 1.0\n\tpkgrel = 1\n\tarch = any\n\npkgname = demo\n\tpkgdesc = Sub package\n";
        assert_eq!(metadata_description(srcinfo).as_deref(), Some("Top level"));
    }

    /// No description anywhere stays missing rather than becoming blank.
    #[test]
    fn no_description_anywhere_stays_missing() {
        let srcinfo = "pkgbase = demo\n\tpkgver = 1.0\n\tpkgrel = 1\n\tarch = any\n\npkgname = one\n\npkgname = two\n";
        assert_eq!(metadata_description(srcinfo), None);
    }
}
