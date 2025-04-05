//! This is where the [`Protocol`](Protocol) structs and its dependencies go.
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// This contains every supported protocol by Arch Linux mirror status as of the time of writing
/// (05/20/2021).
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Protocol {
    /// The HTTP protocol.
    #[serde(rename = "http")]
    Http,

    /// The HTTPS protocol.
    #[serde(rename = "https")]
    Https,

    /// The rsync protocol.
    #[serde(rename = "rsync")]
    Rsync,

    /// The ftp protocol.
    #[serde(rename = "ftp")]
    Ftp,
}

impl FromStr for Protocol {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            "rsync" => Ok(Self::Rsync),
            "ftp" => Ok(Self::Ftp),
            other => Err(anyhow!("Invalid Protocol: {}", other)),
        }
    }
}
