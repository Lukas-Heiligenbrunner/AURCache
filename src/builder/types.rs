use crate::db::versions;

#[derive(Clone)]
pub enum Action {
    Build(String, String, String, versions::ActiveModel),
}
