use crate::pkginfo::parser::Pkginfo;
use std::fmt::{Display, Formatter};

pub struct Desc {
    pub filename: String,
    pub name: String,
    pub base: String,
    pub version: String,
    pub desc: String,
    pub groups: Vec<String>,
    pub csize: String,
    pub isize: String,
    pub md5sum: String,
    pub sha256sum: String,
    pub pgpsig: String,
    pub url: String,
    pub licenses: Vec<String>,
    pub arch: String,
    pub builddate: String,
    pub packager: String,
    pub replace: Vec<String>,
    pub conflicts: Vec<String>,
    pub provides: Vec<String>,
    pub depends: Vec<String>,
    pub optdepends: Vec<String>,
    pub makedepends: Vec<String>,
    pub checkdepends: Vec<String>,
}

impl Display for Desc {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let desc_lines = vec![
            self.add_desc_entry("filename", self.filename.clone()),
            self.add_desc_entry("name", self.name.clone()),
            self.add_desc_entry("base", self.base.clone()),
            self.add_desc_entry("version", self.version.clone()),
            self.add_desc_entry("desc", self.desc.clone()),
            self.add_desc_entries("groups", &self.groups),
            self.add_desc_entry("csize", self.csize.clone()),
            self.add_desc_entry("isize", self.isize.clone()),
            self.add_desc_entry("md5sum", self.md5sum.clone()),
            self.add_desc_entry("sha256sum", self.sha256sum.clone()),
            self.add_desc_entry("pgpsig", self.pgpsig.clone()),
            self.add_desc_entry("url", self.url.clone()),
            self.add_desc_entries("license", &self.licenses),
            self.add_desc_entry("arch", self.arch.clone()),
            self.add_desc_entry("builddate", self.builddate.clone()),
            self.add_desc_entry("packager", self.packager.clone()),
            self.add_desc_entries("replaces", &self.replace),
            self.add_desc_entries("conflicts", &self.conflicts),
            self.add_desc_entries("provides", &self.provides),
            self.add_desc_entries("depends", &self.depends),
            self.add_desc_entries("optdepends", &self.optdepends),
            self.add_desc_entries("makedepends", &self.makedepends),
            self.add_desc_entries("checkdepends", &self.checkdepends),
        ];
        write!(f, "{}", desc_lines.join(""))
    }
}
impl Desc {
    fn add_desc_entry(&self, header: &str, value: String) -> String {
        if !value.is_empty() {
            format!("%{}%\n{}\n\n", header.to_uppercase(), value)
        } else {
            String::new()
        }
    }

    fn add_desc_entries(&self, header: &str, values: &[String]) -> String {
        if values.is_empty()
            || (values.len() == 1
                && values
                    .first()
                    .expect("Must be populated bc. of short-circuit evaluation")
                    .is_empty())
        {
            String::new()
        } else {
            self.add_desc_entry(header, values.join("\n"))
        }
    }
}

impl From<Pkginfo> for Desc {
    fn from(value: Pkginfo) -> Self {
        Desc {
            filename: "".to_string(),
            name: value.pkgname,
            base: value.pkgbase,
            version: value.pkgver,
            desc: value.pkgdesc,
            isize: value.size.to_string(),
            md5sum: "".to_string(),
            csize: "".to_string(),
            url: value.url,
            arch: value.arch,
            builddate: value.builddate,
            packager: value.packager,
            pgpsig: value.pgpsig,
            groups: value.groups,
            licenses: value.licenses,
            replace: value.replaces,
            conflicts: value.conflicts,
            provides: value.provides,
            depends: value.depends,
            optdepends: value.optdepends,
            makedepends: value.makedepends,
            checkdepends: value.checkdepends,
            sha256sum: "".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desc_to_string() {
        let desc = Desc {
            filename: "myfilename".to_string(),
            name: "myname".to_string(),
            base: "mybase".to_string(),
            version: "vers".to_string(),
            desc: "test".to_string(),
            groups: vec!["firstgroup".to_string(), "secgroup".to_string()],
            csize: "test".to_string(),
            isize: "test".to_string(),
            md5sum: "test".to_string(),
            sha256sum: "test".to_string(),
            pgpsig: "test".to_string(),
            url: "test".to_string(),
            licenses: vec!["test".to_string()],
            arch: "test".to_string(),
            builddate: "test".to_string(),
            packager: "test".to_string(),
            replace: vec!["test".to_string()],
            conflicts: vec!["test".to_string()],
            provides: vec!["test".to_string()],
            depends: vec!["test".to_string()],
            optdepends: vec!["test".to_string()],
            makedepends: vec!["test".to_string()],
            checkdepends: vec!["test".to_string()],
        };

        let expected = "\
%FILENAME%
myfilename

%NAME%
myname

%BASE%
mybase

%VERSION%
vers

%DESC%
test

%GROUPS%
firstgroup
secgroup

%CSIZE%
test

%ISIZE%
test

%MD5SUM%
test

%SHA256SUM%
test

%PGPSIG%
test

%URL%
test

%LICENSE%
test

%ARCH%
test

%BUILDDATE%
test

%PACKAGER%
test

%REPLACES%
test

%CONFLICTS%
test

%PROVIDES%
test

%DEPENDS%
test

%OPTDEPENDS%
test

%MAKEDEPENDS%
test

%CHECKDEPENDS%
test

";
        assert_eq!(desc.to_string(), expected);
    }

    #[test]
    fn test_from_pkginfo() {
        let pkginfo = Pkginfo {
            pkgname: "myname".to_string(),
            pkgbase: "mybase".to_string(),
            pkgver: "vers".to_string(),
            pkgdesc: "test".to_string(),
            groups: vec!["firstgroup".to_string(), "secgroup".to_string()],
            size: 1024,
            url: "test".to_string(),
            arch: "test".to_string(),
            builddate: "test".to_string(),
            packager: "test".to_string(),
            pgpsig: "test".to_string(),
            licenses: vec!["test".to_string()],
            replaces: vec!["test".to_string()],
            conflicts: vec!["test".to_string()],
            provides: vec!["test".to_string()],
            depends: vec!["test".to_string()],
            optdepends: vec!["test".to_string()],
            makedepends: vec!["test".to_string()],
            checkdepends: vec!["test".to_string()],
        };

        let desc = Desc::from(pkginfo);

        assert_eq!(desc.filename, "");
        assert_eq!(desc.name, "myname");
        assert_eq!(desc.base, "mybase");
        assert_eq!(desc.version, "vers");
        assert_eq!(desc.desc, "test");
        assert_eq!(
            desc.groups,
            vec!["firstgroup".to_string(), "secgroup".to_string()]
        );
        assert_eq!(desc.csize, "");
        assert_eq!(desc.isize, "1024");
        assert_eq!(desc.md5sum, "");
        assert_eq!(desc.sha256sum, "");
        assert_eq!(desc.pgpsig, "test");
        assert_eq!(desc.url, "test");
        assert_eq!(desc.licenses, vec!["test".to_string()]);
        assert_eq!(desc.arch, "test");
        assert_eq!(desc.builddate, "test");
        assert_eq!(desc.packager, "test");
        assert_eq!(desc.replace, vec!["test".to_string()]);
        assert_eq!(desc.conflicts, vec!["test".to_string()]);
        assert_eq!(desc.provides, vec!["test".to_string()]);
        assert_eq!(desc.depends, vec!["test".to_string()]);
        assert_eq!(desc.optdepends, vec!["test".to_string()]);
    }
}
