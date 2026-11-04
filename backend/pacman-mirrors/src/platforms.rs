use sea_orm::DeriveValueType;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize, DeriveValueType)]
#[sea_orm(value_type = "String")]
pub enum Platform {
    X86_64,
    Aarch64,
    Armv7h,
}

impl Platform {
    /// Every platform AURCache builds for.
    ///
    /// Here rather than spelled out at each use, so that adding one is a change
    /// in this file alone -- the callers that iterate architectures (mirrorlist
    /// configuration, official-repo caches) then pick it up for free.
    pub const ALL: [Self; 3] = [Self::X86_64, Self::Aarch64, Self::Armv7h];

    /// Returns the string representation of the platform.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::Armv7h => "armv7h",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Implements conversion from a &str to a Platform.
/// The error [`Platform::from_str`] returns.
///
/// A named type rather than a `&'static str`: sea-orm 2.0 requires the
/// conversion error behind `DeriveValueType` to be a real `std::error::Error`,
/// and carrying the rejected value means the message can say what was wrong
/// rather than only that something was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePlatformError(String);

impl std::fmt::Display for ParsePlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown platform '{}'", self.0)
    }
}

impl std::error::Error for ParsePlatformError {}

impl FromStr for Platform {
    type Err = ParsePlatformError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            "armv7h" => Ok(Self::Armv7h),
            _ => Err(ParsePlatformError(s.to_string())),
        }
    }
}
