use log::debug;
use rocket::get;
use rocket::http::{Cookie, CookieJar, SameSite};
use rocket::response::Redirect;
use rocket_oauth2::{OAuth2, TokenResponse};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(oauth_login, oauth_callback))]
pub struct AuthApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Redirect to oidc login endpoint"),
    )
)]
#[get("/login")]
pub fn oauth_login(oauth2: OAuth2<()>, cookies: &CookieJar<'_>) -> Redirect {
    oauth2.get_redirect(cookies, &["user:email"]).unwrap()
}

#[utoipa::path(
    responses(
            (status = 200, description = "Oauth callback (called by oidc provider)"),
    )
)]
#[get("/auth")]
pub fn oauth_callback(token: TokenResponse<()>, cookies: &CookieJar<'_>) -> Redirect {
    debug!("Token: {:?}", token);
    cookies.add_private(
        Cookie::build(("token", token.access_token().to_string()))
            .same_site(SameSite::Lax)
            .build(),
    );
    Redirect::to("/")
}
