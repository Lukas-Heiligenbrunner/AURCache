use crate::db::{builds, packages};

#[derive(Clone)]
pub enum Action {
    Build(Box<packages::Model>, Box<builds::Model>),
    Cancel(i32),
}

#[derive(Clone, Debug)]
pub struct BuildStates {}

impl BuildStates {
    pub const ACTIVE_BUILD: i32 = 0;
    pub const SUCCESSFUL_BUILD: i32 = 1;
    pub const FAILED_BUILD: i32 = 2;
    pub const ENQUEUED_BUILD: i32 = 3;
}
