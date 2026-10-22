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
        write_entry(f, "filename", &self.filename)?;
        write_entry(f, "name", &self.name)?;
        write_entry(f, "base", &self.base)?;
        write_entry(f, "version", &self.version)?;
        write_entry(f, "desc", &self.desc)?;
        write_entries(f, "groups", &self.groups)?;
        write_entry(f, "csize", &self.csize)?;
        write_entry(f, "isize", &self.isize)?;
        write_entry(f, "md5sum", &self.md5sum)?;
        write_entry(f, "sha256sum", &self.sha256sum)?;
        write_entry(f, "pgpsig", &self.pgpsig)?;
        write_entry(f, "url", &self.url)?;
        write_entries(f, "license", &self.licenses)?;
        write_entry(f, "arch", &self.arch)?;
        write_entry(f, "builddate", &self.builddate)?;
        write_entry(f, "packager", &self.packager)?;
        write_entries(f, "replaces", &self.replace)?;
        write_entries(f, "conflicts", &self.conflicts)?;
        write_entries(f, "provides", &self.provides)?;
        write_entries(f, "depends", &self.depends)?;
        write_entries(f, "optdepends", &self.optdepends)?;
        write_entries(f, "makedepends", &self.makedepends)?;
        write_entries(f, "checkdepends", &self.checkdepends)
    }
}

/// Write one `%HEADER%` section; an empty value writes nothing at all.
fn write_entry(f: &mut Formatter<'_>, header: &str, value: &str) -> std::fmt::Result {
    if value.is_empty() {
        return Ok(());
    }
    write!(f, "%{}%\n{value}\n\n", header.to_uppercase())
}

/// Write a multi-value `%HEADER%` section. An empty list, or a list holding
/// nothing but one empty string, writes nothing at all.
fn write_entries(f: &mut Formatter<'_>, header: &str, values: &[String]) -> std::fmt::Result {
    match values {
        [] => Ok(()),
        [value] if value.is_empty() => Ok(()),
        _ => write_entry(f, header, &values.join("\n")),
    }
}

impl From<Pkginfo> for Desc {
    fn from(value: Pkginfo) -> Self {
        Self {
            filename: String::new(),
            name: value.pkgname,
            base: value.pkgbase,
            version: value.pkgver,
            desc: value.pkgdesc,
            isize: value.size.to_string(),
            md5sum: String::new(),
            csize: String::new(),
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
            sha256sum: String::new(),
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
