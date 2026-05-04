use std::str::FromStr;

pub trait ParseSetting: Sized + Clone {
    fn parse_setting(s: &str) -> Result<Self, String>;
}
macro_rules! impl_parse_setting {
    ($($ty:ty),* $(,)?) => {
        $(
            impl ParseSetting for $ty {
                fn parse_setting(s: &str) -> Result<Self, String> {
                    s.parse::<$ty>().map_err(|e| e.to_string())
                }
            }
        )*
    };
}

impl_parse_setting!(u32, i32, u64, i64, String);

impl<T> ParseSetting for Option<T>
where
    T: FromStr + Clone,
    <T as FromStr>::Err: std::fmt::Display,
{
    fn parse_setting(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            Ok(None)
        } else {
            s.parse::<T>().map(Some).map_err(|e| e.to_string())
        }
    }
}
