use anyhow::anyhow;
use sea_orm::ActiveValue;

pub trait ActiveValueExt<T> {
    fn get(&self) -> anyhow::Result<&T>;
}

impl<T> ActiveValueExt<T> for ActiveValue<T>
where
    T: Clone,
    sea_orm::Value: From<T>,
{
    fn get(&self) -> anyhow::Result<&T> {
        match self {
            ActiveValue::Set(value) | ActiveValue::Unchanged(value) => Ok(value),
            _ => Err(anyhow!("Value is not set")),
        }
    }
}
