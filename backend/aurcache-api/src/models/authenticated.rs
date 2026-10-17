use crate::auth::username_for_api_token;
use rocket::Request;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use sea_orm::DatabaseConnection;

#[derive(Debug, Clone)]
pub struct OauthEnabled(pub bool);

#[derive(Debug)]
pub struct Authenticated {
    pub username: Option<String>,
}

#[derive(Debug)]
pub enum LoginError {
    InvalidData,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Authenticated {
    type Error = LoginError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let oauth_enabled = req
            .rocket()
            .state::<OauthEnabled>()
            .unwrap_or(&OauthEnabled(false));
        if oauth_enabled.0 {
            // A valid session cookie authenticates on its own; the username
            // cookie is only used to label the session.
            if req.cookies().get_private("token").is_some() {
                let username = req
                    .cookies()
                    .get_private("username")
                    .map(|cookie| cookie.value().to_string());
                return Outcome::Success(Self { username });
            }

            // The scheme is case-insensitive (RFC 7235); some clients send
            // `bearer` or `BEARER`.
            let bearer_token = req
                .headers()
                .get_one("Authorization")
                .and_then(|value| value.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, token)| token.trim())
                .filter(|token| !token.is_empty());

            let Some(bearer_token) = bearer_token else {
                return Outcome::Error((Status::Unauthorized, LoginError::InvalidData));
            };

            let Some(db) = req.rocket().state::<DatabaseConnection>() else {
                return Outcome::Error((Status::InternalServerError, LoginError::InvalidData));
            };

            match username_for_api_token(db, bearer_token).await {
                Ok(Some(username)) => Outcome::Success(Self {
                    username: Some(username),
                }),
                Ok(None) => Outcome::Error((Status::Unauthorized, LoginError::InvalidData)),
                Err(_) => Outcome::Error((Status::InternalServerError, LoginError::InvalidData)),
            }
        } else {
            Outcome::Success(Self { username: None })
        }
    }
}
