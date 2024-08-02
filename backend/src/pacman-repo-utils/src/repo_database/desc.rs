use crate::pkginfo::parser::Pkginfo;

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

impl Desc {
    pub fn to_string(&self) -> String {
        let mut desc_lines = vec![];
        desc_lines.push(self.add_desc_entry("filename", self.filename.clone()));
        desc_lines.push(self.add_desc_entry("NAME", self.name.clone()));
        desc_lines.push(self.add_desc_entry("BASE", self.base.clone()));
        desc_lines.push(self.add_desc_entry("VERSION", self.version.clone()));
        desc_lines.push(self.add_desc_entry("DESC", self.desc.clone()));
        desc_lines.push(self.add_desc_entries("GROUPS", &self.groups));
        desc_lines.push(self.add_desc_entry("CSIZE", self.csize.clone()));
        desc_lines.push(self.add_desc_entry("ISIZE", self.isize.clone()));
        desc_lines.push(self.add_desc_entry("MD5SUM", self.md5sum.clone()));
        desc_lines.push(self.add_desc_entry("SHA256SUM", self.sha256sum.clone()));
        desc_lines.push(self.add_desc_entry("PGPSIG", self.pgpsig.clone()));
        desc_lines.push(self.add_desc_entry("URL", self.url.clone()));
        desc_lines.push(self.add_desc_entries("LICENSE", &self.licenses));
        desc_lines.push(self.add_desc_entry("ARCH", self.arch.clone()));
        desc_lines.push(self.add_desc_entry("BUILDDATE", self.builddate.clone()));
        desc_lines.push(self.add_desc_entry("PACKAGER", self.packager.clone()));
        desc_lines.push(self.add_desc_entries("REPLACES", &self.replace));
        desc_lines.push(self.add_desc_entries("CONFLICTS", &self.conflicts));
        desc_lines.push(self.add_desc_entries("PROVIDES", &self.provides));
        desc_lines.push(self.add_desc_entries("DEPENDS", &self.depends));
        desc_lines.push(self.add_desc_entries("OPTDEPENDS", &self.optdepends));
        desc_lines.push(self.add_desc_entries("MAKEDEPENDS", &self.makedepends));
        desc_lines.push(self.add_desc_entries("CHECKDEPENDS", &self.checkdepends));

        desc_lines.join("")
    }

    fn add_desc_entry(&self, header: &str, value: String) -> String {
        format!("%{}%\n{}\n\n", header, value)
    }

    fn add_desc_entries(&self, header: &str, values: &Vec<String>) -> String {
        if values.is_empty() {
            return String::new();
        }
        if values.len() == 1 && values.first().unwrap().eq("") {
            return String::new();
        }
        format!("%{}%\n{}\n\n", header, values.join("\n"))
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
