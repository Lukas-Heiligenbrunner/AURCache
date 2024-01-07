use anyhow::anyhow;
use nom::branch::{alt, permutation};
use nom::bytes::complete::{tag, take_until, take_while1};
use nom::character::complete::space0;
use nom::multi::separated_list1;
use nom::IResult;

#[derive(Debug, PartialEq)]
pub struct PkgBuild {
    //mandatory fields
    pub pkgname: Vec<String>,
    pub pkgver: String,
    pub pkgrel: String,
    pub arch: Vec<String>,
}

// parsing mandatory fields in any order
pub fn parse(input: String) -> anyhow::Result<PkgBuild> {
    let i = input.clone();
    let result = permutation((parse_pkgname, parse_pkgver, parse_pkgrel, parse_arch))(&i).map(
        |(next_input, (pkgname, pkgver, pkgrel, arch))| {
                PkgBuild {
                    pkgname,
                    pkgver,
                    pkgrel,
                    arch,
                }
        },
    );
    result.map_err(|err| anyhow!(err.to_owned()))
}

fn parse_field<'a>(input: &'a str, field: &str) -> IResult<&'a str, String> {
    let (input, _) = take_until(format!("{field}=").as_str())(input)?;
    let (input, _) = tag(format!("{field}=").as_str())(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = tag("=")(input)?;
    let (input, _) = space0(input)?;
    let (input, value) = take_until("\n")(input)?;
    let (input, _) = tag("\n")(input)?;
    Ok((input, value.to_string()))
}

pub fn parse_pkgname(input: &str) -> IResult<&str, Vec<String>> {
    let (input, _) = take_until("pkgname=")(input)?;
    let (input, _) = tag("pkgname=")(input)?;
    let (input, _) = space0(input)?;
    alt((parse_pkgname_multiple, parse_pkgname_single))(input)
}

fn parse_pkgname_single(input: &str) -> IResult<&str, Vec<String>> {
    let (input, value) = take_until("\n")(input)?;
    let (_, value) = alt((single_quoted, |v| Ok((v, v))))(value)?;
    let (input, _) = tag("\n")(input)?;
    Ok((input, vec![value.to_string()]))
}

fn parse_pkgname_multiple(input: &str) -> IResult<&str, Vec<String>> {
    let (input, _) = tag("(")(input)?;
    let (input, pkgnames) = take_until(")")(input)?;
    let (input, _) = tag(")")(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = tag("\n")(input)?;
    let names: Vec<&str> = pkgnames
        .split_whitespace()
        .map(|x| match single_quoted(x) {
            Ok(v) => v.1,
            Err(_) => x,
        })
        .collect();
    Ok((input, names.iter().map(|s| s.to_string()).collect()))
}

pub fn parse_pkgver(input: &str) -> IResult<&str, String> {
    parse_field(input, "pkgver")
}

pub fn parse_pkgrel(input: &str) -> IResult<&str, String> {
    parse_field(input, "pkgrel")
}

pub fn parse_arch(input: &str) -> IResult<&str, Vec<String>> {
    let (input, _) = take_until("arch=")(input)?;
    let (input, _) = tag("arch=")(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = tag("(")(input)?;
    let (input, arches) = take_until(")")(input)?;
    let (_, arches) = separated_list1(tag(" "), single_quoted)(arches)?;
    let (input, _) = tag(")")(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = tag("\n")(input)?;
    Ok((input, arches.iter().map(|s| s.to_string()).collect()))
}

fn single_quoted(input: &str) -> IResult<&str, &str> {
    let (input, _) = tag("'")(input)?;
    let (input, value) = take_while1(|c| c != '\'')(input)?;
    let (input, _) = tag("'")(input)?;
    Ok((input, value))
}

#[cfg(test)]
mod tests {
    use crate::aur::pkgbuild::{parse, parse_arch, parse_pkgname, parse_pkgrel, parse_pkgver};
    use std::path::Path;

    #[test]
    fn pkgname() {
        let input = "pkgname=foo\n";
        let expected = vec!["foo"];
        let (input, pkgname) = parse_pkgname(input).unwrap();
        assert_eq!(input, "");
        assert_eq!(pkgname, expected);
    }

    #[test]
    fn pkgname_quoted() {
        let input = "pkgname='foo'\n";
        let expected = vec!["foo"];
        let (input, pkgname) = parse_pkgname(input).unwrap();
        assert_eq!(input, "");
        assert_eq!(pkgname, expected);
    }

    #[test]
    fn pkgname_multi() {
        let input = "pkgname=(foo bar)\n";
        let expected = vec!["foo", "bar"];
        let (input, pkgname) = parse_pkgname(input).unwrap();
        assert_eq!(input, "");
        assert_eq!(pkgname, expected);
    }

    #[test]
    fn pkgname_multi_quoted() {
        let input = "pkgname=('foo' 'bar')\n";
        let expected = vec!["foo", "bar"];
        let (input, pkgname) = parse_pkgname(input).unwrap();
        assert_eq!(input, "");
        assert_eq!(pkgname, expected);
    }

    #[test]
    fn pkgver() {
        let input = "pkgver=1.0\n";
        let expected = "1.0";
        let (input, pkgver) = parse_pkgver(input).unwrap();
        assert_eq!(input, "");
        assert_eq!(pkgver, expected);
    }

    #[test]
    fn pkgrel() {
        let input = "pkgrel=1\n";
        let expected = "1";
        let (input, pkgrel) = parse_pkgrel(input).unwrap();
        assert_eq!(input, "");
        assert_eq!(pkgrel, expected);
    }

    #[test]
    fn arch() {
        let input = "arch=('i686' 'x86_64')\n";
        let expected = vec!["i686".to_string(), "x86_64".to_string()];
        let (input, arch) = parse_arch(input).unwrap();
        assert_eq!(input, "");
        assert_eq!(arch, expected);
    }

    #[test]
    fn pkgbuild() {
        let input = "pkgname=foo\npkgver=1.0\njbr_ver=17.0.8.1\npkgrel=1\narch=('i686' 'x86_64')\n".to_string();
        let expected = super::PkgBuild {
            pkgname: vec!["foo".to_string()],
            pkgver: "1.0".to_string(),
            pkgrel: "1".to_string(),
            arch: vec!["i686".to_string(), "x86_64".to_string()],
        };
        let pkgbuild = parse(input.clone()).unwrap();
        assert_eq!(pkgbuild, expected);
    }

    #[test]
    fn pkgsssbuild() {
        let input = r#"
# Maintainer: D. Can Celasun <can[at]dcc[dot]im>
# Co-Maintainer: Urs Wolfer <uwolfer @ fwo.ch>


pkgbase=intellij-idea-ultimate-edition
pkgname=(intellij-idea-ultimate-edition intellij-idea-ultimate-edition-jre)
pkgver=2023.3.2
pkgrel=1
_buildver=233.13135.103
jbr_ver=17.0.8.1
jbr_build=aarch64-b1059
jbr_minor=3
arch=('x86_64' 'aarch64')
pkgdesc="An intelligent IDE for Java, Groovy and other programming languages with advanced refactoring features intensely focused on developer productivity."
url="https://www.jetbrains.com/idea/"
license=('custom:commercial')
options=(!strip)
source=("https://download-cf.jetbrains.com/idea/ideaIU-$pkgver.tar.gz"
        "jetbrains-idea.desktop")
source_aarch64=("https://cache-redirector.jetbrains.com/intellij-jbr/jbr-$jbr_ver-linux-$jbr_build.$jbr_minor.tar.gz"
                "https://github.com/JetBrains/intellij-community/raw/master/bin/linux/aarch64/fsnotifier")
sha256sums=('c763926c0bd1d14a1a9f07846a3cfd0330d5eacce31263c453710ac7a0f4c20f'
            '83af2ba8f9f14275a6684e79d6d4bd9b48cd852c047dacfc81324588fa2ff92b')
sha256sums_aarch64=('edb2526aacb789f5442c47893dab324000aece64ef49771f228079c9395aadbf'
                    'eb3c61973d34f051dcd3a9ae628a6ee37cd2b24a1394673bb28421a6f39dae29')

prepare() {
  # Extract the JRE from the main pacakge
  if [ -d "$srcdir"/jbr ]; then
    rm -rf "$srcdir"/jbr
  fi

  # https://youtrack.jetbrains.com/articles/IDEA-A-48/JetBrains-IDEs-on-AArch64#linux
  if [ "${CARCH}" == "aarch64" ]; then
    cp -a "$srcdir"/jbr-${jbr_ver}-linux-${jbr_build}.${jbr_minor} "$srcdir"/jbr
    cp -f fsnotifier "$srcdir"/idea-IU-$_buildver/bin/fsnotifier
    chmod +x "$srcdir"/idea-IU-$_buildver/bin/fsnotifier
    rm -rf "$srcdir"/idea-IU-$_buildver/jbr
  else
    mv "$srcdir"/idea-IU-$_buildver/jbr "$srcdir"/jbr
  fi
}

package_intellij-idea-ultimate-edition() {
  backup=("opt/${pkgname}/bin/idea64.vmoptions" "opt/${pkgname}/bin/idea.properties")
  depends=('giflib' 'libxtst' 'libxrender')
  optdepends=(
    'intellij-idea-ultimate-edition-jre: JetBrains custom JRE (Recommended)' 'java-environment: Required if intellij-idea-ultimate-edition-jre is not installed'
    'libdbusmenu-glib: For global menu support'
  )

  cd "$srcdir"

  install -d "$pkgdir"/{opt/$pkgname,usr/bin}
  mv idea-IU-${_buildver}/* "$pkgdir"/opt/$pkgbase

  # https://youtrack.jetbrains.com/issue/IDEA-185828
  chmod +x "$pkgdir"/opt/$pkgbase/plugins/maven/lib/maven3/bin/mvn

  ln -s /opt/$pkgname/bin/idea.sh "$pkgdir"/usr/bin/$pkgname
  install -D -m644 "$srcdir"/jetbrains-idea.desktop "$pkgdir"/usr/share/applications/jetbrains-idea.desktop
  install -D -m644 "$pkgdir"/opt/$pkgbase/bin/idea.svg "$pkgdir"/usr/share/pixmaps/"$pkgname".svg

  # workaround FS#40934
  sed -i 's|lcd|on|'  "$pkgdir"/opt/$pkgname/bin/*.vmoptions
}

package_intellij-idea-ultimate-edition-jre() {
  url="https://github.com/JetBrains/JetBrainsRuntime"
  install -d -m 755 "$pkgdir"/opt/$pkgbase
  mv "$srcdir"/jbr "$pkgdir"/opt/$pkgbase
}

# vim:set ts=2 sw=2 et:
        ""#;
        let expected = super::PkgBuild {
            pkgname: vec!["foo".to_string()],
            pkgver: "1.0".to_string(),
            pkgrel: "1".to_string(),
            arch: vec!["i686".to_string(), "x86_64".to_string()],
        };
        let pkgbuild = parse(input.to_string()).unwrap();
        assert_eq!(pkgbuild, expected);
    }
}
