use anyhow::bail;
use base64::Engine;
use base64::engine::general_purpose;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::{fs, io};

/// Upper bound on a detached signature, mirroring `repo-add`'s own check.
const MAX_SIGNATURE_BYTES: usize = 16384;

#[derive(Default)]
pub struct Pkginfo {
    pub groups: Vec<String>,
    pub licenses: Vec<String>,
    pub replaces: Vec<String>,
    pub depends: Vec<String>,
    pub conflicts: Vec<String>,
    pub provides: Vec<String>,
    pub optdepends: Vec<String>,
    pub makedepends: Vec<String>,
    pub checkdepends: Vec<String>,
    pub pkgname: String,
    pub pkgbase: String,
    pub pkgver: String,
    pub pkgdesc: String,
    pub size: u64,
    pub url: String,
    pub arch: String,
    pub builddate: String,
    pub packager: String,
    pub pgpsig: String,
}

impl Pkginfo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn parse(&mut self, file: impl Read) -> anyhow::Result<()> {
        let reader = io::BufReader::new(file);
        for line in reader.lines() {
            self.parse_line(&line?)?;
        }
        Ok(())
    }

    pub fn parse_line(&mut self, line: &str) -> anyhow::Result<()> {
        if line.starts_with('#') {
            return Ok(());
        }
        let Some((key, value)) = line.split_once('=') else {
            return Ok(());
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "group" => self.groups.push(value.to_string()),
            "license" => self.licenses.push(value.to_string()),
            "replaces" => self.replaces.push(value.to_string()),
            "depend" => self.depends.push(value.to_string()),
            "conflict" => self.conflicts.push(value.to_string()),
            "provides" => self.provides.push(value.to_string()),
            "optdepend" => self.optdepends.push(value.to_string()),
            "makedepend" => self.makedepends.push(value.to_string()),
            "checkdepend" => self.checkdepends.push(value.to_string()),
            "pkgname" => self.pkgname = value.to_string(),
            "pkgbase" => self.pkgbase = value.to_string(),
            "pkgver" => self.pkgver = value.to_string(),
            "pkgdesc" => self.pkgdesc = value.to_string(),
            "size" => self.size = value.parse()?,
            "url" => self.url = value.to_string(),
            "arch" => self.arch = value.to_string(),
            "builddate" => self.builddate = value.to_string(),
            "packager" => self.packager = value.to_string(),
            _ => {}
        }
        Ok(())
    }

    pub fn set_signature(&mut self, pkgfile: &Path) -> anyhow::Result<()> {
        let mut sigfile = pkgfile.as_os_str().to_owned();
        sigfile.push(".sig");
        let sigfile = PathBuf::from(sigfile);
        if !sigfile.exists() {
            return Ok(());
        }

        let sigdata = fs::read(&sigfile)?;
        if sigdata.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
            bail!(
                "Cannot use armored signatures for packages: {}",
                sigfile.display()
            );
        }
        if sigdata.len() > MAX_SIGNATURE_BYTES {
            bail!("Package signature file too large: {}", sigfile.display());
        }

        self.pgpsig = general_purpose::STANDARD.encode(&sigdata);
        Ok(())
    }

    pub fn valid(&self) -> bool {
        // Ensure $pkgname and $pkgver variables were found
        !self.pkgname.is_empty() && !self.pkgver.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_missing() {
        let mut pkginfo = Pkginfo::new();
        assert!(!pkginfo.valid());
        pkginfo.pkgname = "test".to_string();
        assert!(!pkginfo.valid());
        pkginfo.pkgver = "1.0".to_string();
        assert!(pkginfo.valid());
    }

    #[test]
    fn parse() {
        let mut pkginfo = Pkginfo::new();
        let data = r#"
            pkgname = test
            pkgver = 1.0
            pkgdesc = test package
            size = 1024
            url = https://example.com
            arch = x86_64
            builddate = 2021-01-01
            packager = test
            "#;
        pkginfo.parse(data.as_bytes()).unwrap();
        assert_eq!(pkginfo.pkgname, "test");
        assert_eq!(pkginfo.pkgver, "1.0");
        assert_eq!(pkginfo.pkgdesc, "test package");
        assert_eq!(pkginfo.size, 1024);
        assert_eq!(pkginfo.url, "https://example.com");
        assert_eq!(pkginfo.arch, "x86_64");
        assert_eq!(pkginfo.builddate, "2021-01-01");
        assert_eq!(pkginfo.packager, "test");
    }

    #[test]
    fn test_large_size_parse() {
        let mut pkginfo = Pkginfo::new();
        let data = r#"
            size = 6000000000
            "#;
        pkginfo.parse(data.as_bytes()).unwrap();
        assert_eq!(pkginfo.size, 6000000000);
    }

    #[test]
    fn parse_array() {
        let mut pkginfo = Pkginfo::new();
        let data = r#"
            group = test
            group = secgroup
            license = MIT
            replaces = test
            depend = test
            conflict = test
            provides = test
            optdepend = myoptdep
            optdepend = secopdep
            makedepend = test
            checkdepend = test
            "#;
        pkginfo.parse(data.as_bytes()).unwrap();
        assert_eq!(pkginfo.groups, vec!["test", "secgroup"]);
        assert_eq!(pkginfo.licenses, vec!["MIT"]);
        assert_eq!(pkginfo.replaces, vec!["test"]);
        assert_eq!(pkginfo.depends, vec!["test"]);
        assert_eq!(pkginfo.conflicts, vec!["test"]);
        assert_eq!(pkginfo.provides, vec!["test"]);
        assert_eq!(pkginfo.optdepends, vec!["myoptdep", "secopdep"]);
        assert_eq!(pkginfo.makedepends, vec!["test"]);
        assert_eq!(pkginfo.checkdepends, vec!["test"]);
    }
}
