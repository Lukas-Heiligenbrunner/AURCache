//! POSIX shell argument rendering. Pass argv directly when no shell is needed.

/// Render one argument so a POSIX shell recovers exactly its original bytes.
#[must_use]
pub fn quote_argument(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_=:./,@+".contains(c))
    {
        argument.to_owned()
    } else {
        format!("'{}'", argument.replace('\'', "'\"'\"'"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn shell_recovers_each_argument_without_expansion() {
        for argument in ["", "hello", "a b", "a'b", "\"$HOME`id`$(id);\n*\\", "λ"] {
            let output = std::process::Command::new("sh")
                .args(["-c", &format!("printf '%s' {}", quote_argument(argument))])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, argument.as_bytes());
        }
    }
}
