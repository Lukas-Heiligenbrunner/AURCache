use rocket_oauth2::{OAuthConfig, StaticProvider};
use std::env;

/// Env var naming the OAuth users allowed to sign in.
pub const ALLOWED_USERS_ENV: &str = "OAUTH_ALLOWED_USERS";

pub fn oauth_config_from_env() -> anyhow::Result<OAuthConfig> {
    // ensure OAUTH_USERINFO_URI is also available
    env::var("OAUTH_USERINFO_URI")?;

    Ok(OAuthConfig::new(
        StaticProvider {
            auth_uri: env::var("OAUTH_AUTH_URI")?.into(),
            token_uri: env::var("OAUTH_TOKEN_URI")?.into(),
        },
        env::var("OAUTH_CLIENT_ID")?,
        env::var("OAUTH_CLIENT_SECRET")?,
        Some(env::var("OAUTH_REDIRECT_URI")?),
    ))
}

/// The sign-in allowlist, or `None` when the deployment has not set one.
///
/// `None` means "no restriction" -- the behaviour of every deployment that has
/// not opted in -- which is why this is an `Option` rather than an empty list
/// standing for "nobody". A variable set to whitespace or to nothing but
/// separators is treated as unset, so clearing the value re-opens sign-in
/// instead of locking everyone out.
///
/// Entries are separated by a comma or a semicolon: a comma is what an operator
/// writing a list of addresses reaches for, and a semicolon is what the rest of
/// AURCache's list-valued variables use. Neither can occur in an address, so
/// accepting both costs nothing.
pub fn allowed_users() -> Option<Vec<String>> {
    parse_allowed_users(env::var(ALLOWED_USERS_ENV).ok().as_deref())
}

/// [`allowed_users`] split from the environment, so it can be tested.
fn parse_allowed_users(raw: Option<&str>) -> Option<Vec<String>> {
    let entries: Vec<String> = raw?
        .split([',', ';'])
        .map(|entry| entry.trim().to_lowercase())
        .filter(|entry| !entry.is_empty())
        .collect();
    (!entries.is_empty()).then_some(entries)
}

/// Whether `email` may sign in, given the configured allowlist.
///
/// Compared case-insensitively: providers are inconsistent about the case they
/// report an address in, and an operator who types their own address in a
/// different case than their provider does should not be locked out of their
/// own server.
///
/// An absent email is refused whenever a list is configured. The list names
/// addresses, so a user we cannot name an address for is not on it -- and
/// failing open here would mean a provider that stops returning the claim
/// silently disables the restriction.
pub fn is_user_allowed(allowed: Option<&[String]>, email: Option<&str>) -> bool {
    let Some(allowed) = allowed else {
        return true;
    };
    let Some(email) = email else {
        return false;
    };
    let email = email.trim().to_lowercase();
    allowed.iter().any(|entry| entry == &email)
}

#[cfg(test)]
mod tests {
    use super::{is_user_allowed, parse_allowed_users};

    /// Not opting in must not restrict anything, and neither must a variable
    /// left blank -- an operator clearing the value is re-opening sign-in, not
    /// locking every user out including themselves.
    #[test]
    fn an_unset_or_blank_list_restricts_nobody() {
        assert_eq!(parse_allowed_users(None), None);
        assert_eq!(parse_allowed_users(Some("")), None);
        assert_eq!(parse_allowed_users(Some("   ")), None);
        assert_eq!(parse_allowed_users(Some(" , ; ")), None);
        assert!(is_user_allowed(None, Some("anyone@example.com")));
        assert!(is_user_allowed(None, None));
    }

    #[test]
    fn entries_are_split_on_either_separator_and_trimmed() {
        assert_eq!(
            parse_allowed_users(Some(" a@example.com , b@example.com ")),
            Some(vec![
                "a@example.com".to_string(),
                "b@example.com".to_string()
            ])
        );
        assert_eq!(
            parse_allowed_users(Some("a@example.com;b@example.com")),
            Some(vec![
                "a@example.com".to_string(),
                "b@example.com".to_string()
            ])
        );
    }

    #[test]
    fn a_listed_address_is_allowed_whatever_case_it_arrives_in() {
        let allowed = parse_allowed_users(Some("Me@Example.COM")).unwrap();
        assert!(is_user_allowed(Some(&allowed), Some("me@example.com")));
        assert!(is_user_allowed(Some(&allowed), Some("  ME@EXAMPLE.COM  ")));
    }

    /// The point of the feature: everyone else is refused.
    #[test]
    fn an_unlisted_address_is_refused() {
        let allowed = parse_allowed_users(Some("me@example.com")).unwrap();
        assert!(!is_user_allowed(Some(&allowed), Some("someone@else.com")));
    }

    /// Fail closed. A provider that stops returning the claim must not silently
    /// turn the restriction off.
    #[test]
    fn a_user_with_no_email_is_refused_when_a_list_is_set() {
        let allowed = parse_allowed_users(Some("me@example.com")).unwrap();
        assert!(!is_user_allowed(Some(&allowed), None));
        assert!(!is_user_allowed(Some(&allowed), Some("   ")));
    }
}
