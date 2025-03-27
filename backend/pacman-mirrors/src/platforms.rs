use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    X86_64,
    Aarch64,
    Armv7h,
}

impl Platform {
    /// Returns the string representation of the platform.
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::X86_64 => "x86_64",
            Platform::Aarch64 => "aarch64",
            Platform::Armv7h => "armv7h",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Implements conversion from a &str to a Platform.
impl FromStr for Platform {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "x86_64" => Ok(Platform::X86_64),
            "aarch64" => Ok(Platform::Aarch64),
            "armv7h" => Ok(Platform::Armv7h),
            _ => Err("Unknown platform"),
        }
    }
}

/// A wrapper type that can be iterated over to yield all Platform variants.
pub struct Platforms;

impl IntoIterator for Platforms {
    type Item = Platform;
    type IntoIter = std::array::IntoIter<Platform, 3>;

    fn into_iter(self) -> Self::IntoIter {
        [Platform::X86_64, Platform::Aarch64, Platform::Armv7h].into_iter()
    }
}
