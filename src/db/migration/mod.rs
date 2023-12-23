pub use sea_orm_migration::prelude::*;

mod create;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(create::Migration)]
    }
}
