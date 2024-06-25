use rocket::fs::NamedFile;
use rocket::http::uri::Segments;
use rocket::http::{Method, Status};
use rocket::response::Responder;
use rocket::route::{Handler, Outcome};
use rocket::{async_trait, figment, Data, Request, Response, Route};
use rocket_seek_stream::SeekStream;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CustomFileServer {
    root: PathBuf,
    rank: isize,
}

impl CustomFileServer {
    /// The default rank use by `FileServer` routes.
    const DEFAULT_RANK: isize = 10;

    #[track_caller]
    pub fn from<P: AsRef<Path>>(path: P) -> Self {
        CustomFileServer::new(path)
    }

    #[track_caller]
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let path = path.as_ref();
        CustomFileServer {
            root: path.into(),
            rank: Self::DEFAULT_RANK,
        }
    }

    pub fn rank(mut self, rank: isize) -> Self {
        self.rank = rank;
        self
    }
}

impl From<CustomFileServer> for Vec<Route> {
    fn from(server: CustomFileServer) -> Self {
        let source = figment::Source::File(server.root.clone());
        let mut route = Route::ranked(server.rank, Method::Get, "/<path..>", server);
        route.name = Some(format!("FileServer: {}", source).into());
        vec![route]
    }
}

#[async_trait]
impl Handler for CustomFileServer {
    async fn handle<'r>(&self, req: &'r Request<'_>, data: Data<'r>) -> Outcome<'r> {
        use rocket::http::uri::fmt::Path;

        let path = req
            .segments::<Segments<'_, Path>>(0..)
            .ok()
            .and_then(|segments| segments.to_path_buf(true).ok())
            .map(|path| self.root.join(path));

        let response = match path {
            None => {None},
            Some(p) => NamedFile::open(p)
                .await
                .ok()
                .and_then(|file| file.respond_to(req).ok()),
        };

        match response {
            None => Outcome::forward(data, Status::NotFound),
            Some(file) => Outcome::Success(file),
        }
    }
}
