use crate::models::authenticated::Authenticated;
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

        match file {
            None => Outcome::Error(Status::NotFound),
            Some(file_content) => {
                let content_type: ContentType = path
                    .extension()
                    .map(|x| x.to_string_lossy())
                    .and_then(|x| ContentType::from_extension(&x))
                    .unwrap_or(ContentType::Plain);
                let rsp = Response::build()
                    .header(content_type)
                    .sized_body(file_content.data.len(), Cursor::new(file_content.data))
                    .finalize();
                Outcome::Success(rsp)
            }
        }
    }
}
