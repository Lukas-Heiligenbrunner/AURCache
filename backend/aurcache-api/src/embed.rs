use crate::auth::Authenticated;
use crate::ui_cache::{self, CachePolicy};
use rocket::http::uri::Segments;
use rocket::http::uri::fmt::Path;
use rocket::http::{ContentType, Method, Status};
use rocket::request::FromRequest;
use rocket::response::{Redirect, Responder};
use rocket::route::{Handler, Outcome};
use rocket::{Data, Request, Response, Route};
use rust_embed::RustEmbed;
use std::io::Cursor;
use std::path::PathBuf;
use tracing::error;

#[derive(RustEmbed)]
#[folder = "web"]
struct Asset;

#[derive(Clone)]
pub struct CustomHandler;

impl From<CustomHandler> for Vec<Route> {
    fn from(handler: CustomHandler) -> Self {
        vec![Route::ranked(-2, Method::Get, "/<path..>", handler)]
    }
}

#[rocket::async_trait]
impl Handler for CustomHandler {
    async fn handle<'r>(&self, request: &'r Request<'_>, _: Data<'r>) -> Outcome<'r> {
        if Authenticated::from_request(request).await.is_error() {
            return match Redirect::to("/api/login").respond_to(request) {
                Ok(r) => Outcome::Success(r),
                Err(e) => {
                    error!("Failed to redirect: {e:?}");
                    Outcome::Error(Status::InternalServerError)
                }
            };
        }

        let Some(mut path) = request
            .segments::<Segments<'_, Path>>(0..)
            .ok()
            .and_then(|segments| segments.to_path_buf(true).ok())
        else {
            return Outcome::Error(Status::BadRequest);
        };

        // A frontend route matches no file. Answering it with the app shell is
        // what makes a reload or a pasted deep link work; see `crate::spa`.
        // The root is the shell too, so it needs no case of its own.
        let mut file = Asset::get(path.to_string_lossy().as_ref());
        if file.is_none() && crate::spa::serves_app_shell(&path) {
            path = PathBuf::from("index.html");
            file = Asset::get("index.html");
        }

        let Some(file_content) = file else {
            return Outcome::Error(Status::NotFound);
        };

        // Sent on the 304 as well: it refreshes what the browser stored.
        // `Vary` because the compression fairing picks an encoding from
        // `Accept-Encoding` and does not say so itself.
        // The compression fairing needs the media type on a 304 too, so it
        // applies the same exclusions as on the cached 200 response.
        let content_type: ContentType = path
            .extension()
            .map(|x| x.to_string_lossy())
            .and_then(|x| ContentType::from_extension(&x))
            .unwrap_or(ContentType::Plain);
        let etag = ui_cache::etag(&file_content.metadata.sha256_hash());
        let mut response = Response::build();
        response
            .header(content_type)
            .raw_header("ETag", etag.clone())
            .raw_header("Cache-Control", CachePolicy::of(&path).header())
            .raw_header("Vary", "Accept-Encoding");

        if ui_cache::not_modified(request.headers().get_one("If-None-Match"), &etag) {
            return Outcome::Success(response.status(Status::NotModified).finalize());
        }

        Outcome::Success(
            response
                .sized_body(file_content.data.len(), Cursor::new(file_content.data))
                .finalize(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::config::OAuthSettings;
    use rocket::http::Header;
    use rocket::local::asynchronous::Client;
    use rocket_async_compression::{Compression, Level};

    #[rocket::async_test]
    async fn revalidation_preserves_the_cached_content_encoding() {
        let client = Client::tracked(
            rocket::build()
                .manage(None::<OAuthSettings>)
                .attach(Compression::with_level(Level::Precise(4)))
                .mount("/", CustomHandler),
        )
        .await
        .unwrap();

        for path in ["/icon.svg", "/index.html"] {
            for encoding in ["identity", "gzip", "br"] {
                let response = client
                    .get(path)
                    .header(Header::new("Accept-Encoding", encoding))
                    .dispatch()
                    .await;
                assert_eq!(response.status(), Status::Ok);
                let etag = response.headers().get_one("ETag").unwrap().to_owned();
                let expected_encoding = if path.ends_with(".svg") || encoding == "identity" {
                    None
                } else {
                    Some(encoding)
                };
                assert_eq!(
                    response.headers().get_one("Content-Encoding"),
                    expected_encoding,
                    "{path}, {encoding}: initial response"
                );

                let response = client
                    .get(path)
                    .header(Header::new("Accept-Encoding", encoding))
                    .header(Header::new("If-None-Match", etag.clone()))
                    .dispatch()
                    .await;
                assert_eq!(response.status(), Status::NotModified);
                assert_eq!(response.headers().get_one("ETag"), Some(etag.as_str()));
                assert_eq!(
                    response.headers().get_one("Cache-Control"),
                    Some("no-cache")
                );
                assert_eq!(response.headers().get_one("Vary"), Some("Accept-Encoding"));
                assert_eq!(
                    response.headers().get_one("Content-Encoding"),
                    expected_encoding,
                    "{path}, {encoding}: conditional response"
                );
            }
        }
    }
}
