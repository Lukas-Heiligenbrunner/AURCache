use crate::activity_serializer::ActivitySerializer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageUpdateActivity {
    pub package: String,
    pub forced: bool,
}

impl ActivitySerializer for PackageUpdateActivity {
    fn format(&self) -> String {
        if self.forced {
            format!("forced update of package {}", self.package)
        } else {
            format!("updated package {}", self.package)
        }
    }
}
