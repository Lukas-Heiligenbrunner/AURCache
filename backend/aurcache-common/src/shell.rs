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

/// A path as one word of an `ssh_config` value, such as `UserKnownHostsFile`,
/// which ssh splits on whitespace and in which it expands `%`.
///
/// A `${NAME}` in the path cannot be escaped: ssh expands it, and then finds no
/// such file or fails to run, so it fails closed.
#[must_use]
pub fn ssh_config_word(path: &str) -> String {
    let path = path.replace('%', "%%");
    if !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_=:./,@+%".contains(c))
    {
        return path;
    }
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
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

    #[test]
    fn a_path_is_one_word_of_ssh_config() {
        assert_eq!(
            ssh_config_word("/work/scratch/known_hosts"),
            "/work/scratch/known_hosts"
        );
        assert_eq!(ssh_config_word("/a b/c"), "\"/a b/c\"");
        assert_eq!(ssh_config_word("/100%/x"), "/100%%/x");
        assert_eq!(ssh_config_word(r#"/q"\#x"#), r#""/q\"\\#x""#);
    }
}
