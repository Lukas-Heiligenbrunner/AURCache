//! What a log entry refers to: a package, a worker, or one build of a package.
//!
//! A reference travels as `namespace:id` — `pkg:hello`, `worker:builder-01`,
//! `build:hello/7` — so anything reading a stored payload can tell what a value
//! is without consulting a catalogue of event kinds. That is what lets the
//! entity index be built mechanically and the filter run without knowing which
//! kinds exist.
//!
//! The namespace is in the value rather than in the key, so the key is free to
//! be the *role* the entity plays (`old`, `new`, `dependent`). Safety does not
//! come from the encoding, though: it comes from the field's type. A field
//! declared [`PackageRef`] refuses a worker or a build on the way in and on the
//! way out, so a role cannot end up holding the wrong kind of thing.
//!
//! See `design/implemented/structured-logs.md`.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Namespace of a package reference.
pub const NS_PACKAGE: &str = "pkg";
/// Namespace of a worker reference.
pub const NS_WORKER: &str = "worker";
/// Namespace of a build reference.
pub const NS_BUILD: &str = "build";

/// A package, by pkgbase. Renders as `pkg:hello`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageRef(pub String);

/// A worker, by the name it reported. Renders as `worker:builder-01`.
///
/// The name, not the fingerprint, because that is what the worker page is
/// addressed by -- and a name shared by two machines resolves to a choice
/// rather than a guess, which is the page's existing behaviour.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerRef(pub String);

/// One build of a package. Renders as `build:hello/7`.
///
/// Both halves, because that is what the build page is addressed by. A pkgbase
/// cannot contain `/` (the AUR allows alphanumerics and `@._+-`), so the split
/// is unambiguous.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BuildRef {
    pub pkgbase: String,
    pub number: i32,
}

/// Any of the three, for the places that handle references without caring which
/// kind they are: the entity index, an entry's scope, and link resolution.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntityRef {
    Package(PackageRef),
    Worker(WorkerRef),
    Build(BuildRef),
}

impl EntityRef {
    /// The namespace this reference lives in, as the entity index stores it.
    #[must_use]
    pub const fn namespace(&self) -> &'static str {
        match self {
            Self::Package(_) => NS_PACKAGE,
            Self::Worker(_) => NS_WORKER,
            Self::Build(_) => NS_BUILD,
        }
    }

    /// The identifier within that namespace, as the entity index stores it:
    /// a pkgbase, a worker name, or `pkgbase/number`.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Self::Package(PackageRef(name)) | Self::Worker(WorkerRef(name)) => name.clone(),
            Self::Build(build) => format!("{}/{}", build.pkgbase, build.number),
        }
    }
    /// How the reference reads in a sentence: the name a person knows it by,
    /// with no namespace -- the sentence around it already says what it is.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Package(PackageRef(name)) | Self::Worker(WorkerRef(name)) => name.clone(),
            Self::Build(build) => format!("{} #{}", build.pkgbase, build.number),
        }
    }
}

impl From<PackageRef> for EntityRef {
    fn from(value: PackageRef) -> Self {
        Self::Package(value)
    }
}

impl From<WorkerRef> for EntityRef {
    fn from(value: WorkerRef) -> Self {
        Self::Worker(value)
    }
}

impl From<BuildRef> for EntityRef {
    fn from(value: BuildRef) -> Self {
        Self::Build(value)
    }
}

impl From<&str> for PackageRef {
    fn from(name: &str) -> Self {
        Self(name.to_string())
    }
}

impl From<String> for PackageRef {
    fn from(name: String) -> Self {
        Self(name)
    }
}

impl From<&str> for WorkerRef {
    fn from(name: &str) -> Self {
        Self(name.to_string())
    }
}

impl From<String> for WorkerRef {
    fn from(name: String) -> Self {
        Self(name)
    }
}

// ---------------------------------------------------------------------------
// Rendering and parsing
// ---------------------------------------------------------------------------

impl fmt::Display for PackageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{NS_PACKAGE}:{}", self.0)
    }
}

impl fmt::Display for WorkerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{NS_WORKER}:{}", self.0)
    }
}

impl fmt::Display for BuildRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{NS_BUILD}:{}/{}", self.pkgbase, self.number)
    }
}

impl fmt::Display for EntityRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Package(r) => r.fmt(f),
            Self::Worker(r) => r.fmt(f),
            Self::Build(r) => r.fmt(f),
        }
    }
}

/// Split `namespace:id` once, so an id may itself contain a colon -- a worker
/// is called whatever its machine calls it, and nothing forbids one there.
fn split_ref(raw: &str) -> Result<(&str, &str), String> {
    raw.split_once(':')
        .filter(|(ns, id)| !ns.is_empty() && !id.is_empty())
        .ok_or_else(|| format!("{raw:?} is not a `namespace:id` reference"))
}

/// Check a reference is in the namespace the field expects, and return its id.
fn expect_ns<'a>(raw: &'a str, want: &str) -> Result<&'a str, String> {
    let (ns, id) = split_ref(raw)?;
    if ns == want {
        Ok(id)
    } else {
        Err(format!("expected a {want} reference, got {raw:?}"))
    }
}

impl FromStr for PackageRef {
    type Err = String;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        expect_ns(raw, NS_PACKAGE).map(|id| Self(id.to_string()))
    }
}

impl FromStr for WorkerRef {
    type Err = String;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        expect_ns(raw, NS_WORKER).map(|id| Self(id.to_string()))
    }
}

impl BuildRef {
    /// Parse a CLI or URL identity (`pkgbase/number`) without an entity namespace.
    pub fn parse_id(raw: &str) -> Result<Self, String> {
        let (pkgbase, number) = raw
            .rsplit_once('/')
            .ok_or_else(|| format!("expected <pkgbase>/<number>, got {raw:?}"))?;
        if pkgbase.is_empty() {
            return Err(format!("{raw:?} names no package"));
        }
        let number: i32 = number
            .parse()
            .map_err(|_| format!("{raw:?} does not end in a build number"))?;
        if number < 1 {
            return Err("build numbers start at 1".to_string());
        }
        Ok(Self {
            pkgbase: pkgbase.to_owned(),
            number,
        })
    }

    /// Format a CLI or URL identity without the activity-log namespace.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}/{}", self.pkgbase, self.number)
    }
}

impl FromStr for BuildRef {
    type Err = String;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse_id(expect_ns(raw, NS_BUILD)?)
    }
}

impl FromStr for EntityRef {
    type Err = String;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (ns, _) = split_ref(raw)?;
        match ns {
            NS_PACKAGE => PackageRef::from_str(raw).map(Self::Package),
            NS_WORKER => WorkerRef::from_str(raw).map(Self::Worker),
            NS_BUILD => BuildRef::from_str(raw).map(Self::Build),
            other => Err(format!("{other:?} is not a known reference namespace")),
        }
    }
}

// ---------------------------------------------------------------------------
// Serde: every reference is the one string, in and out
// ---------------------------------------------------------------------------

/// Write a reference as its `namespace:id` string.
macro_rules! serialize_as_string {
    ($($t:ty),*) => {$(
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }
    )*};
}

/// Read a reference back, refusing anything from another namespace. This is
/// what makes a mistyped role a parse error rather than a silent oddity.
macro_rules! deserialize_from_string {
    ($($t:ty),*) => {$(
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                raw.parse().map_err(D::Error::custom)
            }
        }
    )*};
}

serialize_as_string!(PackageRef, WorkerRef, BuildRef, EntityRef);
deserialize_from_string!(PackageRef, WorkerRef, BuildRef, EntityRef);

// The schema is the wire form: one string, whatever the Rust type is.
macro_rules! string_schema {
    ($($t:ty => $example:literal),* $(,)?) => {$(
        impl utoipa::PartialSchema for $t {
            fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
                use utoipa::openapi::schema::{ObjectBuilder, Type};
                ObjectBuilder::new()
                    .schema_type(Type::String)
                    .description(Some("An entity reference, written `namespace:id`."))
                    .examples([serde_json::Value::String($example.to_string())])
                    .into()
            }
        }

        impl utoipa::ToSchema for $t {}
    )*};
}

string_schema! {
    PackageRef => "pkg:hello",
    WorkerRef => "worker:builder-01",
    BuildRef => "build:hello/7",
    EntityRef => "pkg:hello",
}

/// One entry, as the API returns it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct LogEntry {
    pub id: i32,
    /// Stable identity of what happened, `domain.verb_object`.
    pub kind: String,
    pub severity: crate::api::activity::Severity,
    /// The sentence as it was rendered when the entry was written. The UI may
    /// re-render from `kind` and `data`; this is what it falls back to, and
    /// what every consumer without a catalogue reads.
    pub message: String,
    /// The typed payload, keyed by the role each value played.
    pub data: serde_json::Value,
    /// What the entry was emitted under, if anything.
    #[serde(default)]
    pub scope: Option<EntityRef>,
    /// Unix seconds.
    pub timestamp: i64,
    /// `None` for anything the server did on its own.
    #[serde(default)]
    pub user: Option<String>,
}

/// One page of the log, and how long the filtered log is.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    pub total: u64,
}

/// What to narrow the log to: what a client asks `/log` for, and what the
/// server queries by. Every field is "show me less"; the default is the whole
/// log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// This severity and worse. Stored as an ordered number, so this is one
    /// comparison rather than a list of kinds the query would have to know.
    pub severity: Option<crate::api::activity::Severity>,
    /// Only what happened since the server last started.
    pub since_boot: bool,
    /// Only entries of this kind.
    pub kind: Option<String>,
    /// Only entries naming this entity, in any role -- or, for a build, also
    /// recorded during it.
    pub entity: Option<EntityRef>,
    /// Narrow [`Self::entity`] to one role: the dependent, rather than any of
    /// the three packages a dependency replacement names.
    pub role: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        serde_json::from_str(&serde_json::to_string(value).expect("serialize"))
            .expect("deserialize")
    }

    #[test]
    fn references_render_as_namespace_and_id() {
        assert_eq!(PackageRef::from("hello").to_string(), "pkg:hello");
        assert_eq!(
            WorkerRef::from("builder-01").to_string(),
            "worker:builder-01"
        );
        assert_eq!(
            BuildRef {
                pkgbase: "hello".into(),
                number: 7
            }
            .to_string(),
            "build:hello/7"
        );
    }

    #[test]
    fn references_round_trip_through_json() {
        let package = PackageRef::from("gtk3+");
        assert_eq!(round_trip(&package), package);

        let worker = WorkerRef::from("builder-01");
        assert_eq!(round_trip(&worker), worker);

        let build = BuildRef {
            pkgbase: "lib32-glibc".into(),
            number: 42,
        };
        assert_eq!(round_trip(&build), build);

        let erased = EntityRef::Build(build);
        assert_eq!(round_trip(&erased), erased);
    }

    /// The whole point of typing the field: a role declared for one kind of
    /// entity cannot be read as holding another.
    #[test]
    fn a_reference_refuses_another_namespace() {
        assert!(serde_json::from_str::<PackageRef>(r#""worker:builder-01""#).is_err());
        assert!(serde_json::from_str::<PackageRef>(r#""build:hello/7""#).is_err());
        assert!(serde_json::from_str::<WorkerRef>(r#""pkg:hello""#).is_err());
        assert!(serde_json::from_str::<BuildRef>(r#""pkg:hello""#).is_err());
        assert!(serde_json::from_str::<EntityRef>(r#""kitten:mittens""#).is_err());
    }

    /// A worker is called whatever its machine calls it, and nothing forbids a
    /// colon or a slash there -- so the namespace splits once, from the left.
    #[test]
    fn a_worker_name_may_contain_the_separator() {
        let odd = WorkerRef::from("ci:runner/2");
        assert_eq!(odd.to_string(), "worker:ci:runner/2");
        assert_eq!(round_trip(&odd), odd);
        assert_eq!(
            "worker:ci:runner/2".parse::<EntityRef>().unwrap(),
            EntityRef::Worker(odd)
        );
    }

    /// A pkgbase cannot contain `/`, so the build number is what follows the
    /// last one and the split is unambiguous.
    #[test]
    fn a_build_splits_on_its_number() {
        let build: BuildRef = "build:hello/7".parse().unwrap();
        assert_eq!(build.pkgbase, "hello");
        assert_eq!(build.number, 7);

        for bad in ["build:hello", "build:hello/", "build:/7", "build:hello/x"] {
            assert!(bad.parse::<BuildRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_malformed_reference_is_refused() {
        for bad in ["", "hello", ":hello", "pkg:", "pkg"] {
            assert!(bad.parse::<EntityRef>().is_err(), "{bad:?}");
        }
    }

    /// What the entity index stores. The namespace and the id are what every
    /// query is written against, so they have to survive the erasure.
    #[test]
    fn the_index_sees_a_namespace_and_an_id() {
        let package = EntityRef::from(PackageRef::from("hello"));
        assert_eq!(package.namespace(), NS_PACKAGE);
        assert_eq!(package.id(), "hello");

        let build = EntityRef::from(BuildRef {
            pkgbase: "hello".into(),
            number: 7,
        });
        assert_eq!(build.namespace(), NS_BUILD);
        assert_eq!(build.id(), "hello/7");

        // And the pair reads back as the reference it came from.
        assert_eq!(
            format!("{}:{}", build.namespace(), build.id())
                .parse::<EntityRef>()
                .unwrap(),
            build
        );
    }
}
