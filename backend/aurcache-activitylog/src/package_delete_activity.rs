use crate::activity_serializer::ActivitySerializer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageDeleteActivity {
    pub package: String,
}

impl ActivitySerializer for PackageDeleteActivity {
    fn format(&self) -> String {
        format!("deleted package {}", self.package)
    }
}
