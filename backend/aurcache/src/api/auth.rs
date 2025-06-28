use anyhow::Context;
use log::debug;
use reqwest::header::AUTHORIZATION;
use rocket::get;
use rocket::http::{Cookie, CookieJar, SameSite};
use rocket::response::Redirect;
use rocket::response::status::Unauthorized;
use rocket_oauth2::{OAuth2, TokenResponse};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(oauth_login, oauth_callback))]
pub struct AuthApi;

#[derive(serde::Deserialize, Debug)]
pub struct OauthUserInfo {
    //pub email: String,
    pub name: String,
    //pub preferred_username: String,
    //pub nickname: String,
}

#[utoipa::path(
    responses(
            (status = 200, description = "Redirect to oidc login endpoint"),
    )
)]
#[get("/login")]
pub fn oauth_login(oauth2: OAuth2<OauthUserInfo>, cookies: &CookieJar<'_>) -> Redirect {
    oauth2
        .get_redirect(cookies, &["profile", "openid", "email"])
        .unwrap()
}

#[utoipa::path(
    responses(
            (status = 200, description = "Oauth callback (called by oidc provider)"),
    )
)]
#[get("/auth")]
pub async fn oauth_callback(
    token: TokenResponse<OauthUserInfo>,
    cookies: &CookieJar<'_>,
) -> Result<Redirect, Unauthorized<String>> {
    cookies.add_private(
        Cookie::build(("token", token.access_token().to_string()))
            .same_site(SameSite::Lax)
            .build(),
    );

    let user_info: OauthUserInfo = reqwest::Client::builder()
        .build()
        .context("failed to build reqwest client")
        .map_err(|e| Unauthorized(e.to_string()))?
        .get(std::env::var("OAUTH_USERINFO_URI").map_err(|e| Unauthorized(e.to_string()))?)
        .header(AUTHORIZATION, format!("Bearer {}", token.access_token()))
        .send()
        .await
        .context("failed to complete request")
        .map_err(|e| Unauthorized(e.to_string()))?
        .json()
        .await
        .context("failed to deserialize response")
        .map_err(|e| Unauthorized(e.to_string()))?;

    let real_name = user_info.name;
    debug!("Logged in username: {}", real_name);

    // Set a private cookie with the user's name, and redirect to the home page.
    cookies.add_private(
        Cookie::build(("username", real_name.to_string()))
            .same_site(SameSite::Lax)
            .build(),
    );

    Ok(Redirect::to("/"))
}
