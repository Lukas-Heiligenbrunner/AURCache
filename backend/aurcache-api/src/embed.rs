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
        let etag = ui_cache::etag(&file_content.metadata.sha256_hash());
        let mut response = Response::build();
        response
            .raw_header("ETag", etag.clone())
            .raw_header("Cache-Control", CachePolicy::of(&path).header())
            .raw_header("Vary", "Accept-Encoding");

        if ui_cache::not_modified(request.headers().get_one("If-None-Match"), &etag) {
            return Outcome::Success(response.status(Status::NotModified).finalize());
        }

        let content_type: ContentType = path
            .extension()
            .map(|x| x.to_string_lossy())
            .and_then(|x| ContentType::from_extension(&x))
            .unwrap_or(ContentType::Plain);
        Outcome::Success(
            response
                .header(content_type)
                .sized_body(file_content.data.len(), Cursor::new(file_content.data))
                .finalize(),
        )
    }
}
