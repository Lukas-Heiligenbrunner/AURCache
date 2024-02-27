use crate::db::{builds, versions};

#[derive(Clone)]
pub enum Action {
    Build(
        String,
        String,
        String,
        versions::ActiveModel,
        builds::ActiveModel,
    ),
    Cancel(i32),
}
