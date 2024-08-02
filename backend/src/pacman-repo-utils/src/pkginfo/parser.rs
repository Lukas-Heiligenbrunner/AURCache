use anyhow::anyhow;
use base64::engine::general_purpose;
use base64::Engine;
use std::io::{BufRead, Read};
use std::path::Path;
use std::{fs, io};

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
    pub size: u32,
    pub url: String,
    pub arch: String,
    pub builddate: String,
    pub packager: String,
    pub pgpsig: String,
}

impl Pkginfo {
    pub fn new() -> Self {
        Self {
            groups: vec![],
            licenses: vec![],
            replaces: vec![],
            depends: vec![],
            conflicts: vec![],
            provides: vec![],
            optdepends: vec![],
            makedepends: vec![],
            checkdepends: vec![],
            pkgname: String::new(),
            pkgbase: String::new(),
            pkgver: String::new(),
            pkgdesc: String::new(),
            size: 0,
            url: String::new(),
            arch: String::new(),
            builddate: String::new(),
            packager: String::new(),
            pgpsig: String::new(),
        }
    }

    pub fn parse(&mut self, file: impl Read) -> anyhow::Result<()> {
        let reader = io::BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            self.parse_line(line);
        }
        Ok(())
    }

    pub fn parse_line(&mut self, line: String) {
        if line.starts_with('#') {
            return;
        }
        let (key, value) = match line.split_once('=') {
            None => return,
            Some((key, value)) => (key.trim(), value.trim()),
        };
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
            "size" => self.size = value.parse().unwrap_or(0),
            "url" => self.url = value.to_string(),
            "arch" => self.arch = value.to_string(),
            "builddate" => self.builddate = value.to_string(),
            "packager" => self.packager = value.to_string(),
            _ => {}
        }
    }

    pub fn set_signature(&mut self, pkgfile: &str) -> anyhow::Result<()> {
        let sigfile = format!("{}.sig", pkgfile);
        if Path::new(&sigfile).exists() {
            let sigdata = fs::read(&sigfile)?;
            if sigdata.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
                eprintln!("Cannot use armored signatures for packages: {}", sigfile);
                return Err(anyhow!("Invalid package signature file"));
            }
            let pgpsigsize = sigdata.len();
            if pgpsigsize > 16384 {
                eprintln!("Invalid package signature file '{}'.", sigfile);
                return Err(anyhow!("Invalid package signature file"));
            }

            self.pgpsig = general_purpose::STANDARD.encode(&sigdata);
        }
        Ok(())
    }
}
