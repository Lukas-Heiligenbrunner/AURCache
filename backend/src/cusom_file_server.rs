use std::path::{PathBuf, Path};
use rocket::{async_trait, Data, figment, Request, Route};
use rocket::futures::StreamExt;
use rocket::http::Method;
use rocket::http::uri::Segments;
use rocket::route::{Handler, Outcome};
use rocket_seek_stream::SeekStream;

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
        CustomFileServer { root: path.into(), rank: Self::DEFAULT_RANK }
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

        let path = req.segments::<Segments<'_, Path>>(0..).ok()
            .and_then(|segments| segments.to_path_buf(true).ok())
            .map(|path| self.root.join(path));

        match path {
            Some(p) => Outcome::from_or_forward(req, data, SeekStream::from_path(p).ok()),
            None => Outcome::forward(data),
        }
    }
}
