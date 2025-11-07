use crate::activity_serializer::ActivitySerializer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageAddActivity {
    pub package: String,
}

impl ActivitySerializer for PackageAddActivity {
    fn format(&self) -> String {
        format!("added package {}", self.package)
    }
}
