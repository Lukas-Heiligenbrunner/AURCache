#[derive(Clone)]
pub struct SettingsEntry {
    pub key: &'static str,
    pub env_name: &'static str,
    pub default: &'static str,
}
