use rocket::http::uri::fmt::Path;
use rocket::http::uri::Segments;
use rocket::http::{ContentType, Method, Status};
use rocket::route::{Handler, Outcome};
use rocket::{Data, Request, Response, Route};
use rust_embed::{RustEmbed};
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
        let mut path = request
            .segments::<Segments<'_, Path>>(0..)
            .ok()
            .and_then(|segments| segments.to_path_buf(true).ok())
            .unwrap();

        if path.is_dir() || path.to_str() == Some("") {
            path = path.join("index.html")
        }

        match <Asset as RustEmbed>::get(path.to_string_lossy().as_ref()) {
            None => Outcome::Failure(Status::NotFound),
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
