use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket::Request;
use rocket_okapi::OpenApiFromRequest;

#[derive(Debug, Clone)]
pub struct OauthEnabled(pub bool);

#[derive(OpenApiFromRequest)]
pub struct Authenticated;

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
            req.cookies()
                .get_private("token")
                .and_then(|cookie| cookie.value().parse().ok())
                .map_or_else(
                    || Outcome::Error((Status::Unauthorized, LoginError::InvalidData)),
                    |_: String| Outcome::Success(Authenticated),
                )
        } else {
            Outcome::Success(Authenticated)
        }
    }
}
