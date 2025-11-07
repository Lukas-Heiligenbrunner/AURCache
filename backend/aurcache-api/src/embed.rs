use crate::api::models::authenticated::Authenticated;
use log::error;
use rocket::http::uri::Segments;
use rocket::http::uri::fmt::Path;
use rocket::http::{ContentType, Method, Status};
use rocket::request::FromRequest;
use rocket::response::{Redirect, Responder};
use rocket::route::{Handler, Outcome};
use rocket::{Data, Request, Response, Route};
use rust_embed::RustEmbed;
use std::io::Cursor;

#[derive(RustEmbed)]
#[folder = "web"]
struct Asset;

#[derive(Clone)]
pub struct CustomHandler {}

impl Into<Vec<Route>> for CustomHandler {
    fn into(self) -> Vec<Route> {
        vec![Route::ranked(-2, Method::Get, "/<path..>", self)]
    }
}

#[rocket::async_trait]
impl Handler for CustomHandler {
    async fn handle<'r>(&self, request: &'r Request<'_>, _: Data<'r>) -> Outcome<'r> {
        if Authenticated::from_request(request).await.is_error() {
            return match Redirect::to("/api/login").respond_to(request) {
                Ok(r) => Outcome::Success(r),
                Err(e) => {
                    error!("Failed to redirect: {:?}", e);
                    Outcome::Error(Status::InternalServerError)
                }
            };
        }

        let mut path = request
            .segments::<Segments<'_, Path>>(0..)
            .ok()
            .and_then(|segments| segments.to_path_buf(true).ok())
            .unwrap();

        if path.is_dir() || path.to_str() == Some("") {
            path = path.join("index.html")
        }

        // if let None =  path.extension()  {
        //     path = "index.html".into();
        // }

        match <Asset as RustEmbed>::get(path.to_string_lossy().as_ref()) {
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
