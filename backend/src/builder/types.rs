use crate::db::{builds, packages};

#[derive(Clone)]
pub enum Action {
    Build(
        String,
        String,
        String,
        Box<packages::ActiveModel>,
        Box<builds::ActiveModel>,
    ),
    Cancel(i32),
}
