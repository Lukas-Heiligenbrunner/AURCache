use anyhow::Context;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::stats::ApiTokenResponse;
use aurcache_db::api_tokens;
use aurcache_db::prelude::ApiTokens;
use rand::rngs::SysRng;
use rand_core::TryRng;
use rocket::get;
use rocket::http::{Cookie, CookieJar, SameSite, Status};
use rocket::response::Redirect;
use rocket::response::status::Unauthorized;
use rocket::serde::json::Json;
use rocket::{State, post};
use rocket_oauth2::{OAuth2, TokenResponse};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use sha2::{Digest, Sha256};
use tracing::{debug, error};
use utoipa::OpenApi;

use crate::models::authenticated::Authenticated;
use crate::utils::config::{allowed_users, is_user_allowed};
use crate::utils::error::{ApiError, err};

#[derive(OpenApi)]
#[openapi(paths(oauth_login, oauth_callback, regenerate_api_token_endpoint))]
pub struct AuthApi;

#[derive(serde::Deserialize, Debug)]
pub struct OauthUserInfo {
    pub name: String,
    /// The address the sign-in allowlist is matched against.
    ///
    /// Optional because not every provider returns it, and a deployment with no
    /// allowlist has no use for it. When a list *is* configured, an absent
    /// address is refused -- see [`is_user_allowed`].
    pub email: Option<String>,
}

// Intentionally unsalted: unlike passwords, API tokens are generated
// server-side as 256 bits of CSPRNG output (see `generate_api_token`), so
// they have no meaningful entropy to protect against dictionary/rainbow-table
// attacks, and collisions between two users' tokens are not a concern. A
// per-token salt (or slow KDF like bcrypt) would add cost without adding
// security here. A fast, unsalted hash just lets us look up the token by its
// digest without storing the secret itself in the DB.
pub fn hash_api_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn generate_api_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    // `SysRng` is rand 0.10's name for what was `OsRng`, and reading from it is
    // now fallible: the OS can refuse entropy. Refusing to mint a token is the
    // only safe answer -- a token from a degraded source is worse than none --
    // so the failure is returned for the endpoint to report instead of
    // panicking the server.
    SysRng
        .try_fill_bytes(&mut bytes)
        .context("system entropy unavailable; refusing to mint an API token")?;
    Ok(hex::encode(bytes))
}

pub async fn username_for_api_token(
    db: &DatabaseConnection,
    token: &str,
) -> Result<Option<String>, sea_orm::DbErr> {
    ApiTokens::find()
        .filter(api_tokens::Column::TokenHash.eq(hash_api_token(token)))
        .one(db)
        .await
        .map(|token_row| token_row.map(|row| row.username))
}

pub async fn has_api_token(
    db: &DatabaseConnection,
    username: &str,
) -> Result<bool, sea_orm::DbErr> {
    Ok(ApiTokens::find()
        .filter(api_tokens::Column::Username.eq(username))
        .one(db)
        .await?
        .is_some())
}

pub async fn regenerate_api_token(
    db: &DatabaseConnection,
    username: &str,
) -> anyhow::Result<String> {
    let token = generate_api_token()?;
    let token_hash = hash_api_token(&token);

    if let Some(existing) = ApiTokens::find()
        .filter(api_tokens::Column::Username.eq(username))
        .one(db)
        .await?
    {
        let mut active: api_tokens::ActiveModel = existing.into();
        active.token_hash = Set(token_hash);
        active.save(db).await?;
    } else {
        api_tokens::ActiveModel {
            username: Set(username.to_string()),
            token_hash: Set(token_hash),
            ..Default::default()
        }
        .save(db)
        .await?;
    }

    Ok(token)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Redirect to oidc login endpoint"),
            (status = 500, description = "Failed to build the oidc redirect"),
    )
)]
#[get("/login")]
pub fn oauth_login(
    oauth2: OAuth2<OauthUserInfo>,
    cookies: &CookieJar<'_>,
) -> Result<Redirect, Status> {
    oauth2
        .get_redirect(cookies, &["profile", "openid", "email"])
        .map_err(|e| {
            error!("failed to build oauth redirect: {e}");
            Status::InternalServerError
        })
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
    al: &State<ActivityLog>,
) -> Result<Redirect, Unauthorized<String>> {
    // One process-wide client: building one per login buys nothing (no
    // per-request configuration) and costs a connection pool each time.
    static OAUTH_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = OAUTH_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .build()
            .expect("default reqwest client builds")
    });
    let user_info: OauthUserInfo = client
        .get(std::env::var("OAUTH_USERINFO_URI").map_err(|e| Unauthorized(e.to_string()))?)
        .bearer_auth(token.access_token())
        .send()
        .await
        .context("failed to complete request")
        .map_err(|e| Unauthorized(e.to_string()))?
        .json()
        .await
        .context("failed to deserialize response")
        .map_err(|e| Unauthorized(e.to_string()))?;

    let real_name = user_info.name;
    let email = user_info.email.as_deref();

    // Nothing is written to the cookie jar until the user has been identified
    // *and* allowed. Rocket applies jar changes to the response whatever this
    // function returns, and `Authenticated` treats the mere presence of the
    // `token` cookie as a valid session -- so setting it before the check would
    // hand a refused user a working session along with their rejection.
    if !is_user_allowed(allowed_users().as_deref(), email) {
        // Logged at warn: on a server that restricts sign-in, someone being
        // turned away is worth seeing, and the operator locking themselves out
        // by a typo in the list looks identical from the browser.
        al.emit(Event::SignInRefused {
            user: format!("{real_name} ({})", email.unwrap_or("no email reported")),
        });
        return Err(Unauthorized(
            "This account is not permitted to sign in to this AURCache instance.".to_string(),
        ));
    }

    debug!("Logged in username: {real_name}");

    // Both cookies together: the session, and the name that labels it.
    cookies.add_private(
        Cookie::build(("token", token.access_token().to_string()))
            .same_site(SameSite::Lax)
            .build(),
    );
    cookies.add_private(
        Cookie::build(("username", real_name))
            .same_site(SameSite::Lax)
            .build(),
    );

    Ok(Redirect::to("/"))
}

#[utoipa::path(
    responses(
        (status = 200, description = "Regenerate the signed-in user's API token", body = ApiTokenResponse),
        (status = 401, description = "No authenticated user"),
        (status = 500, description = "Could not mint or store a token"),
    )
)]
#[post("/token/regenerate")]
pub async fn regenerate_api_token_endpoint(
    db: &State<DatabaseConnection>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<Json<ApiTokenResponse>, ApiError> {
    let username = a
        .username
        .ok_or_else(|| err(Status::Unauthorized, "No authenticated user available"))?;
    // A failure here is the server's (database, entropy), not the caller's:
    // answering 401 would log the user out for a fault on our side.
    let token = regenerate_api_token(db, &username)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    al.emit_by(Event::TokenRegenerated {}, Some(username));
    Ok(Json(ApiTokenResponse { token }))
}

#[cfg(test)]
mod tests {
    use super::{OauthUserInfo, has_api_token, regenerate_api_token, username_for_api_token};

    /// A provider that does not return `email` must still sign users in. The
    /// field was added for the allowlist, and making it required would have
    /// broken every deployment whose provider omits it -- the failure would be
    /// a deserialisation error on the callback, i.e. nobody can log in at all.
    #[test]
    fn userinfo_without_an_email_still_parses() {
        let info: OauthUserInfo = serde_json::from_str(r#"{"name":"Alex"}"#).unwrap();
        assert_eq!(info.name, "Alex");
        assert_eq!(info.email, None);
    }

    /// Extra claims are ignored rather than rejected: real providers return far
    /// more than these two fields.
    #[test]
    fn userinfo_keeps_the_email_and_ignores_other_claims() {
        let info: OauthUserInfo = serde_json::from_str(
            r#"{"name":"Alex","email":"me@example.com","sub":"1","nickname":"al"}"#,
        )
        .unwrap();
        assert_eq!(info.email.as_deref(), Some("me@example.com"));
    }

    use aurcache_db::migration::Migrator;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn regenerated_token_replaces_previous_value() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let first = regenerate_api_token(&db, "alice").await.unwrap();
        let second = regenerate_api_token(&db, "alice").await.unwrap();

        assert!(has_api_token(&db, "alice").await.unwrap());
        assert_ne!(first, second);
        assert_eq!(
            username_for_api_token(&db, &first).await.unwrap(),
            None,
            "old token should stop authenticating after regeneration"
        );
        assert_eq!(
            username_for_api_token(&db, &second).await.unwrap(),
            Some("alice".to_string())
        );
    }
}
