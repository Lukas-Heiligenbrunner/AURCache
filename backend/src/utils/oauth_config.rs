use rocket_oauth2::{OAuthConfig, StaticProvider};

pub fn oauth_config_from_env() -> anyhow::Result<OAuthConfig> {
    // ensure OAUTH_USERINFO_URI is also available
    std::env::var("OAUTH_USERINFO_URI")?;

    Ok(OAuthConfig::new(
        StaticProvider {
            auth_uri: std::env::var("OAUTH_AUTH_URI")?.into(),
            token_uri: std::env::var("OAUTH_TOKEN_URI")?.into(),
        },
        std::env::var("OAUTH_CLIENT_ID")?,
        std::env::var("OAUTH_CLIENT_SECRET")?,
        Some(std::env::var("OAUTH_REDIRECT_URI")?),
    ))
}
