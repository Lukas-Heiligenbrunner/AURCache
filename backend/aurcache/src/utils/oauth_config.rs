use rocket_oauth2::{OAuthConfig, StaticProvider};
use std::env;

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
