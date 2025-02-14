use crate::activity_log::activity_serializer::ActivitySerializer;
use rocket::serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageUpdateActivity {
    pub package: String,
    pub forced: bool,
}

impl ActivitySerializer for PackageUpdateActivity {
    fn format(&self) -> String {
        match self.forced {
            true => format!("forced update of package {}", self.package),
            false => format!("updated package {}", self.package),
        }
    }
}
