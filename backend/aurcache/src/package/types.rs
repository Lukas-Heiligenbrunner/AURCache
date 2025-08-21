use rocket::serde::{Serialize, Deserialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackageType {
    Aur = 0,
    Custom = 1,
}

impl From<i32> for PackageType {
    fn from(value: i32) -> Self {
        match value {
            0 => PackageType::Aur,
            1 => PackageType::Custom,
            _ => PackageType::Aur, // Default to AUR for unknown values
        }
    }
}

impl From<PackageType> for i32 {
    fn from(package_type: PackageType) -> Self {
        package_type as i32
    }
}