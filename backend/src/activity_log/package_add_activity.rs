use crate::activity_log::activity_serializer::ActivitySerializer;
use rocket::serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageAdd {
    pub user: Option<String>,
    pub package: String,
}

impl ActivitySerializer for PackageAdd {
    fn serialize(&self) -> String {
        match &self.user {
            Some(user) => format!("{} added package {}", user, self.package),
            None => format!("Unknown user added package {}", self.package),
        }
    }
}
