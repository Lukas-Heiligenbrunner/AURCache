//! This module contains every country listed in the Arch Linux mirror status as of the time of
//! writing (05/20/2021).

/// Create a country. You can supply multiple countries to this, separated by a comma.
///
/// Format:
///
/// ```text
/// </// documentation>
/// <name of the country> (<name of the country in screaming snake case>): <name of the country in title case> => (<the country code>)
/// ```
macro_rules! countries {
    ($($(#[$docs:meta])* $name:literal ($snake_case:ident): $kind:ident => $code:ident),+) => {
        use serde::{Deserialize, Serialize};

        /// The country name.
        #[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Serialize, Deserialize)]
        pub enum Kind {
            $($(#[$docs])* $kind),+,

            /// Country which supports all countries.
            Worldwide,

            /// Any unsupported country.
            Other(String)
        }

        impl From<String> for Kind {
            fn from(kind: String) -> Self {
                match kind.as_str() {
                    $($name => Self::$kind),+,
                    "" => Self::Worldwide,
                    _ => Self::Other(kind)
                }
            }
        }

        impl std::fmt::Display for Kind {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                match self {
                    $(Self::$kind => write!(f, $name)),+,
                    Self::Worldwide => write!(f, "Worldwide"),
                    Self::Other(kind) => write!(f, "{}", kind)
                }
            }
        }

        /// The country code.
        #[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Serialize, Deserialize)]
        pub enum Code {
             $($(#[$docs])* $code),+,

             /// Country code which supports all countries.
             Worldwide,

             /// Any unsupported country code.
             Other(String)
        }

        impl From<String> for Code {
            fn from(code: String) -> Self {
                match code.as_str() {
                    $(stringify!($code) => Self::$code),+,
                    "" => Self::Worldwide,
                    _ => Self::Other(code)
                }
            }
        }

        impl std::fmt::Display for Code {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                match self {
                    $(Self::$code => write!(f, "{}", stringify!($code))),+,
                    Self::Worldwide => write!(f, "Worldwide"),
                    Self::Other(code) => write!(f, "{}", code)
                }
            }
        }


        /// The country.
        #[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Serialize, Deserialize)]
        pub struct Country {
            /// The kind of country.
            pub kind: Kind,

            /// The country code.
            pub code: Code
        }

        impl Country {
            $(
            $(#[$docs])*
            pub const $snake_case: Country = Country { kind: Kind::$kind, code: Code::$code };
            )+
            /// An empty placeholder country.
            pub const WORLDWIDE: Country = Country { kind: Kind::Worldwide, code: Code::Worldwide };

            /// Create a new country. If country or code is empty, this will be set to [`None`](None).
            pub fn new(country: &str, code: &str) -> Self {
                let kind = Kind::from(country.to_string());
                let code = Code::from(code.to_string());

                Self { kind, code }
            }
        }

        impl Default for Country {
            fn default() -> Self {
                Self { kind: Kind::Worldwide, code: Code::Worldwide }
            }
        }
    };
}

countries! {
    /// The country Australia. It's kind is [`Kind::Australia`](Kind::Australia). It's code is [`Code::AU`](Code::AU). It's constant is [`Country::AUSTRALIA`](Country::AUSTRALIA).
    "Australia" (AUSTRALIA): Australia => AU,
    /// The country France. It's kind is [`Kind::France`](Kind::France). It's code is [`Code::FR`](Code::FR). It's constant is [`Country::FRANCE`](Country::FRANCE).
    "France" (FRANCE): France => FR,
    /// The country Germany. It's kind is [`Kind::Germany`](Kind::Germany). It's code is [`Code::DE`](Code::DE). It's constant is [`Country::GERMANY`](Country::GERMANY).
    "Germany" (GERMANY): Germany => DE,
    /// The country United States. It's kind is [`Kind::UnitedStates`](Kind::UnitedStates). It's code is [`Code::US`](Code::US). It's constant is [`Country::UNITED_STATES`](Country::UNITED_STATES).
    "United States" (UNITED_STATES): UnitedStates => US,
    /// The country Hungary. It's kind is [`Kind::Hungary`](Kind::Hungary). It's code is [`Code::HU`](Code::HU). It's constant is [`Country::HUNGARY`](Country::HUNGARY).
    "Hungary" (HUNGARY): Hungary => HU,
    /// The country Ireland. It's kind is [`Kind::Ireland`](Kind::Ireland). It's code is [`Code::IE`](Code::IE). It's constant is [`Country::IRELAND`](Country::IRELAND).
    "Ireland" (IRELAND): Ireland => IE,
    /// The country Netherlands. It's kind is [`Kind::Netherlands`](Kind::Netherlands). It's code is [`Code::NL`](Code::NL). It's constant is [`Country::NETHERLANDS`](Country::NETHERLANDS).
    "Netherlands" (NETHERLANDS): Netherlands => NL,
    /// The country Switzerland. It's kind is [`Kind::Switzerland`](Kind::Switzerland). It's code is [`Code::CH`](Code::CH). It's constant is [`Country::SWITZERLAND`](Country::SWITZERLAND).
    "Switzerland" (SWITZERLAND): Switzerland => CH,
    /// The country Turkey. It's kind is [`Kind::Turkey`](Kind::Turkey). It's code is [`Code::TR`](Code::TR). It's constant is [`Country::TURKEY`](Country::TURKEY).
    "Turkey" (TURKEY): Turkey => TR,
    /// The country United Kingdom. It's kind is [`Kind::UnitedKingdom`](Kind::UnitedKingdom). It's code is [`Code::GB`](Code::GB). It's constant is [`Country::UNITED_KINGDOM`](Country::UNITED_KINGDOM).
    "United Kingdom" (UNITED_KINGDOM): UnitedKingdom => GB,
    /// The country Canada. It's kind is [`Kind::Canada`](Kind::Canada). It's code is [`Code::CA`](Code::CA). It's constant is [`Country::CANADA`](Country::CANADA).
    "Canada" (CANADA): Canada => CA,
    /// The country Norway. It's kind is [`Kind::Norway`](Kind::Norway). It's code is [`Code::NO`](Code::NO). It's constant is [`Country::NORWAY`](Country::NORWAY).
    "Norway" (NORWAY): Norway => NO,
    /// The country Israel. It's kind is [`Kind::Israel`](Kind::Israel). It's code is [`Code::IL`](Code::IL). It's constant is [`Country::ISRAEL`](Country::ISRAEL).
    "Israel" (ISRAEL): Israel => IL,
    /// The country Brazil. It's kind is [`Kind::Brazil`](Kind::Brazil). It's code is [`Code::BR`](Code::BR). It's constant is [`Country::BRAZIL`](Country::BRAZIL).
    "Brazil" (BRAZIL): Brazil => BR,
    /// The country Russia. It's kind is [`Kind::Russia`](Kind::Russia). It's code is [`Code::RU`](Code::RU). It's constant is [`Country::RUSSIA`](Country::RUSSIA).
    "Russia" (RUSSIA): Russia => RU,
    /// The country Chile. It's kind is [`Kind::Chile`](Kind::Chile). It's code is [`Code::CL`](Code::CL). It's constant is [`Country::CHILE`](Country::CHILE).
    "Chile" (CHILE): Chile => CL,
    /// The country Spain. It's kind is [`Kind::Spain`](Kind::Spain). It's code is [`Code::ES`](Code::ES). It's constant is [`Country::SPAIN`](Country::SPAIN).
    "Spain" (SPAIN): Spain => ES,
    /// The country New Caledonia. It's kind is [`Kind::NewCaledonia`](Kind::NewCaledonia). It's code is [`Code::NC`](Code::NC). It's constant is [`Country::NEW_CALEDONIA`](Country::NEW_CALEDONIA).
    "New Caledonia" (NEW_CALEDONIA): NewCaledonia => NC,
    /// The country Greece. It's kind is [`Kind::Greece`](Kind::Greece). It's code is [`Code::GR`](Code::GR). It's constant is [`Country::GREECE`](Country::GREECE).
    "Greece" (GREECE): Greece => GR,
    /// The country India. It's kind is [`Kind::India`](Kind::India). It's code is [`Code::IN`](Code::IN). It's constant is [`Country::INDIA`](Country::INDIA).
    "India" (INDIA): India => IN,
    /// The country Taiwan. It's kind is [`Kind::Taiwan`](Kind::Taiwan). It's code is [`Code::TW`](Code::TW). It's constant is [`Country::TAIWAN`](Country::TAIWAN).
    "Taiwan" (TAIWAN): Taiwan => TW,
    /// The country China. It's kind is [`Kind::China`](Kind::China). It's code is [`Code::CN`](Code::CN). It's constant is [`Country::CHINA`](Country::CHINA).
    "China" (CHINA): China => CN,
    /// The country Belgium. It's kind is [`Kind::Belgium`](Kind::Belgium). It's code is [`Code::BE`](Code::BE). It's constant is [`Country::BELGIUM`](Country::BELGIUM).
    "Belgium" (BELGIUM): Belgium => BE,
    /// The country Portugal. It's kind is [`Kind::Portugal`](Kind::Portugal). It's code is [`Code::PT`](Code::PT). It's constant is [`Country::PORTUGAL`](Country::PORTUGAL).
    "Portugal" (PORTUGAL): Portugal => PT,
    /// The country Denmark. It's kind is [`Kind::Denmark`](Kind::Denmark). It's code is [`Code::DK`](Code::DK). It's constant is [`Country::DENMARK`](Country::DENMARK).
    "Denmark" (DENMARK): Denmark => DK,
    /// The country Japan. It's kind is [`Kind::Japan`](Kind::Japan). It's code is [`Code::JP`](Code::JP). It's constant is [`Country::JAPAN`](Country::JAPAN).
    "Japan" (JAPAN): Japan => JP,
    /// The country Belarus. It's kind is [`Kind::Belarus`](Kind::Belarus). It's code is [`Code::BY`](Code::BY). It's constant is [`Country::BELARUS`](Country::BELARUS).
    "Belarus" (BELARUS): Belarus => BY,
    /// The country Czechia. It's kind is [`Kind::Czechia`](Kind::Czechia). It's code is [`Code::CZ`](Code::CZ). It's constant is [`Country::CZECHIA`](Country::CZECHIA).
    "Czechia" (CZECHIA): Czechia => CZ,
    /// The country Italy. It's kind is [`Kind::Italy`](Kind::Italy). It's code is [`Code::IT`](Code::IT). It's constant is [`Country::ITALY`](Country::ITALY).
    "Italy" (ITALY): Italy => IT,
    /// The country Luxembourg. It's kind is [`Kind::Luxembourg`](Kind::Luxembourg). It's code is [`Code::LU`](Code::LU). It's constant is [`Country::LUXEMBOURG`](Country::LUXEMBOURG).
    "Luxembourg" (LUXEMBOURG): Luxembourg => LU,
    /// The country Ukraine. It's kind is [`Kind::Ukraine`](Kind::Ukraine). It's code is [`Code::UA`](Code::UA). It's constant is [`Country::UKRAINE`](Country::UKRAINE).
    "Ukraine" (UKRAINE): Ukraine => UA,
    /// The country Sweden. It's kind is [`Kind::Sweden`](Kind::Sweden). It's code is [`Code::SE`](Code::SE). It's constant is [`Country::SWEDEN`](Country::SWEDEN).
    "Sweden" (SWEDEN): Sweden => SE,
    /// The country North Macedonia. It's kind is [`Kind::NorthMacedonia`](Kind::NorthMacedonia). It's code is [`Code::MK`](Code::MK). It's constant is [`Country::NORTH_MACEDONIA`](Country::NORTH_MACEDONIA).
    "North Macedonia" (NORTH_MACEDONIA): NorthMacedonia => MK,
    /// The country Slovakia. It's kind is [`Kind::Slovakia`](Kind::Slovakia). It's code is [`Code::SK`](Code::SK). It's constant is [`Country::SLOVAKIA`](Country::SLOVAKIA).
    "Slovakia" (SLOVAKIA): Slovakia => SK,
    /// The country Kazakhstan. It's kind is [`Kind::Kazakhstan`](Kind::Kazakhstan). It's code is [`Code::KZ`](Code::KZ). It's constant is [`Country::KAZAKHSTAN`](Country::KAZAKHSTAN).
    "Kazakhstan" (KAZAKHSTAN): Kazakhstan => KZ,
    /// The country Serbia. It's kind is [`Kind::Serbia`](Kind::Serbia). It's code is [`Code::RS`](Code::RS). It's constant is [`Country::SERBIA`](Country::SERBIA).
    "Serbia" (SERBIA): Serbia => RS,
    /// The country Singapore. It's kind is [`Kind::Singapore`](Kind::Singapore). It's code is [`Code::SG`](Code::SG). It's constant is [`Country::SINGAPORE`](Country::SINGAPORE).
    "Singapore" (SINGAPORE): Singapore => SG,
    /// The country Poland. It's kind is [`Kind::Poland`](Kind::Poland). It's code is [`Code::PL`](Code::PL). It's constant is [`Country::POLAND`](Country::POLAND).
    "Poland" (POLAND): Poland => PL,
    /// The country Romania. It's kind is [`Kind::Romania`](Kind::Romania). It's code is [`Code::RO`](Code::RO). It's constant is [`Country::ROMANIA`](Country::ROMANIA).
    "Romania" (ROMANIA): Romania => RO,
    /// The country Iceland. It's kind is [`Kind::Iceland`](Kind::Iceland). It's code is [`Code::IS`](Code::IS). It's constant is [`Country::ICELAND`](Country::ICELAND).
    "Iceland" (ICELAND): Iceland => IS,
    /// The country Hong Kong. It's kind is [`Kind::HongKong`](Kind::HongKong). It's code is [`Code::HK`](Code::HK). It's constant is [`Country::HONG_KONG`](Country::HONG_KONG).
    "Hong Kong" (HONG_KONG): HongKong => HK,
    /// The country Indonesia. It's kind is [`Kind::Indonesia`](Kind::Indonesia). It's code is [`Code::ID`](Code::ID). It's constant is [`Country::INDONESIA`](Country::INDONESIA).
    "Indonesia" (INDONESIA): Indonesia => ID,
    /// The country South Korea. It's kind is [`Kind::SouthKorea`](Kind::SouthKorea). It's code is [`Code::KR`](Code::KR). It's constant is [`Country::SOUTH_KOREA`](Country::SOUTH_KOREA).
    "South Korea" (SOUTH_KOREA): SouthKorea => KR,
    /// The country Croatia. It's kind is [`Kind::Croatia`](Kind::Croatia). It's code is [`Code::HR`](Code::HR). It's constant is [`Country::CROATIA`](Country::CROATIA).
    "Croatia" (CROATIA): Croatia => HR,
    /// The country Ecuador. It's kind is [`Kind::Ecuador`](Kind::Ecuador). It's code is [`Code::EC`](Code::EC). It's constant is [`Country::ECUADOR`](Country::ECUADOR).
    "Ecuador" (ECUADOR): Ecuador => EC,
    /// The country Vietnam. It's kind is [`Kind::Vietnam`](Kind::Vietnam). It's code is [`Code::VN`](Code::VN). It's constant is [`Country::VIETNAM`](Country::VIETNAM).
    "Vietnam" (VIETNAM): Vietnam => VN,
    /// The country Lithuania. It's kind is [`Kind::Lithuania`](Kind::Lithuania). It's code is [`Code::LT`](Code::LT). It's constant is [`Country::LITHUANIA`](Country::LITHUANIA).
    "Lithuania" (LITHUANIA): Lithuania => LT,
    /// The country Latvia. It's kind is [`Kind::Latvia`](Kind::Latvia). It's code is [`Code::LV`](Code::LV). It's constant is [`Country::LATVIA`](Country::LATVIA).
    "Latvia" (LATVIA): Latvia => LV,
    /// The country Bulgaria. It's kind is [`Kind::Bulgaria`](Kind::Bulgaria). It's code is [`Code::BG`](Code::BG). It's constant is [`Country::BULGARIA`](Country::BULGARIA).
    "Bulgaria" (BULGARIA): Bulgaria => BG,
    /// The country Austria. It's kind is [`Kind::Austria`](Kind::Austria). It's code is [`Code::AT`](Code::AT). It's constant is [`Country::AUSTRIA`](Country::AUSTRIA).
    "Austria" (AUSTRIA): Austria => AT,
    /// The country South Africa. It's kind is [`Kind::SouthAfrica`](Kind::SouthAfrica). It's code is [`Code::ZA`](Code::ZA). It's constant is [`Country::SOUTH_AFRICA`](Country::SOUTH_AFRICA).
    "South Africa" (SOUTH_AFRICA): SouthAfrica => ZA,
    /// The country Finland. It's kind is [`Kind::Finland`](Kind::Finland). It's code is [`Code::FI`](Code::FI). It's constant is [`Country::FINLAND`](Country::FINLAND).
    "Finland" (FINLAND): Finland => FI,
    /// The country Slovenia. It's kind is [`Kind::Slovenia`](Kind::Slovenia). It's code is [`Code::SI`](Code::SI). It's constant is [`Country::SLOVENIA`](Country::SLOVENIA).
    "Slovenia" (SLOVENIA): Slovenia => SI,
    /// The country Bosnia and Herzegovina. It's kind is [`Kind::BosniaAndHerzegovina`](Kind::BosniaAndHerzegovina). It's code is [`Code::BA`](Code::BA). It's constant is [`Country::BOSNIA_AND_HERZEGOVINA`](Country::BOSNIA_AND_HERZEGOVINA).
    "Bosnia and Herzegovina" (BOSNIA_AND_HERZEGOVINA): BosniaAndHerzegovina => BA,
    /// The country New Zealand. It's kind is [`Kind::NewZealand`](Kind::NewZealand). It's code is [`Code::NZ`](Code::NZ). It's constant is [`Country::NEW_ZEALAND`](Country::NEW_ZEALAND).
    "New Zealand" (NEW_ZEALAND): NewZealand => NZ,
    /// The country Thailand. It's kind is [`Kind::Thailand`](Kind::Thailand). It's code is [`Code::TH`](Code::TH). It's constant is [`Country::THAILAND`](Country::THAILAND).
    "Thailand" (THAILAND): Thailand => TH,
    /// The country Iran. It's kind is [`Kind::Iran`](Kind::Iran). It's code is [`Code::IR`](Code::IR). It's constant is [`Country::IRAN`](Country::IRAN).
    "Iran" (IRAN): Iran => IR,
    /// The country Bangladesh. It's kind is [`Kind::Bangladesh`](Kind::Bangladesh). It's code is [`Code::BD`](Code::BD). It's constant is [`Country::BANGLADESH`](Country::BANGLADESH).
    "Bangladesh" (BANGLADESH): Bangladesh => BD,
    /// The country Paraguay. It's kind is [`Kind::Paraguay`](Kind::Paraguay). It's code is [`Code::PY`](Code::PY). It's constant is [`Country::PARAGUAY`](Country::PARAGUAY).
    "Paraguay" (PARAGUAY): Paraguay => PY,
    /// The country Colombia. It's kind is [`Kind::Colombia`](Kind::Colombia). It's code is [`Code::CO`](Code::CO). It's constant is [`Country::COLOMBIA`](Country::COLOMBIA).
    "Colombia" (COLOMBIA): Colombia => CO,
    /// The country Georgia. It's kind is [`Kind::Georgia`](Kind::Georgia). It's code is [`Code::GE`](Code::GE). It's constant is [`Country::GEORGIA`](Country::GEORGIA).
    "Georgia" (GEORGIA): Georgia => GE,
    /// The country Kenya. It's kind is [`Kind::Kenya`](Kind::Kenya). It's code is [`Code::KE`](Code::KE). It's constant is [`Country::KENYA`](Country::KENYA).
    "Kenya" (KENYA): Kenya => KE,
    /// The country Pakistan. It's kind is [`Kind::Pakistan`](Kind::Pakistan). It's code is [`Code::PK`](Code::PK). It's constant is [`Country::PAKISTAN`](Country::PAKISTAN).
    "Pakistan" (PAKISTAN): Pakistan => PK,
    /// The country Moldova. It's kind is [`Kind::Moldova`](Kind::Moldova). It's code is [`Code::MD`](Code::MD). It's constant is [`Country::MOLDOVA`](Country::MOLDOVA).
    "Moldova" (MOLDOVA): Moldova => MD,
    /// The country Estonia. It's kind is [`Kind::Estonia`](Kind::Estonia). It's code is [`Code::EE`](Code::EE). It's constant is [`Country::ESTONIA`](Country::ESTONIA).
    "Estonia" (ESTONIA): Estonia => EE,
    /// The country Mexico. It's kind is [`Kind::Mexico`](Kind::Mexico). It's code is [`Code::MX`](Code::MX). It's constant is [`Country::MEXICO`](Country::MEXICO).
    "Mexico" (MEXICO): Mexico => MX,
    /// The country Monaco. It's kind is [`Kind::Monaco`](Kind::Monaco). It's code is [`Code::MC`](Code::MC). It's constant is [`Country::MONACO`](Country::MONACO).
    "Monaco" (MONACO): Monaco => MC
}
