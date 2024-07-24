use crate::db::{builds, packages};

#[derive(Clone)]
pub enum Action {
    Build(Box<packages::ActiveModel>, Box<builds::ActiveModel>),
    Cancel(i32),
}
