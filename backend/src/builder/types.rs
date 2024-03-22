use crate::db::{builds, versions};

#[derive(Clone)]
pub enum Action {
    Build(
        String,
        String,
        String,
        Box<versions::ActiveModel>,
        Box<builds::ActiveModel>,
    ),
    Cancel(i32),
}
