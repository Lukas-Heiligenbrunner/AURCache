use rocket::fs::NamedFile;
use rocket::http::uri::Segments;
use rocket::http::{Header, Method, Status};
use rocket::response::Responder;
use rocket::route::{Handler, Outcome};
use rocket::{Data, Request, Response, Route, async_trait, figment};
use std::io::{Cursor, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

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
        let relative_path = req
            .segments::<Segments<'_, rocket::http::uri::fmt::Path>>(0..)
            .ok()
            .and_then(|segments| segments.to_path_buf(true).ok());

        // Map uri to filepath
        let file_path = match relative_path {
            Some(p) => self.root.join(p),
            None => return Outcome::forward(data, Status::NotFound),
        };

        // open file
        let named_file = match NamedFile::open(&file_path).await {
            Ok(f) => f,
            Err(_) => return Outcome::forward(data, Status::NotFound),
        };

        let metadata = named_file.metadata().await.ok();
        let file_size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let last_modified = metadata.and_then(|m| m.modified().ok()).map(|mtime| {
            let datetime: chrono::DateTime<chrono::Utc> = mtime.into();
            datetime.to_rfc2822()
        });

        let mut builder = match get_range_header_data(req, file_size, &file_path).await {
            Some((partial_data, start, end)) => {
                // Build a 206 Partial Content response.
                let mut builder = Response::build();
                builder
                    .status(Status::PartialContent)
                    .raw_header(
                        "Content-Range",
                        format!("bytes {}-{}/{}", start, end - 1, file_size),
                    )
                    .raw_header("Accept-Ranges", "bytes")
                    .sized_body(partial_data.len(), Cursor::new(partial_data));
                builder
            }
            None => match named_file.respond_to(req) {
                Ok(resp) => Response::build_from(resp),
                Err(_) => return Outcome::error(Status::InternalServerError),
            },
        };

        // Add Headers
        if let Some(lm) = last_modified {
            builder.header(Header::new("Last-Modified", lm));
        }
        builder.header(Header::new("Accept-Ranges", "bytes"));

        Outcome::Success(builder.finalize())
    }
}

/// get range header and read bytes from file
async fn get_range_header_data(
    req: &Request<'_>,
    file_size: u64,
    file_path: &Path,
) -> Option<(Vec<u8>, u64, u64)> {
    let header = req.headers().get_one("Range")?;
    let (start, end) = parse_range_header(header, file_size)?;
    let data = read_file_range(file_path, start, end).await.ok()?;

    Some((data, start, end))
}

/// Parser for Range header in the form "bytes=start-end".
/// Returns a tuple (start, end) where `end` is exclusive.
/// This version does not support multiple ranges.
fn parse_range_header(header: &str, file_size: u64) -> Option<(u64, u64)> {
    if !header.starts_with("bytes=") {
        return None;
    }
    let range = &header[6..];
    let parts: Vec<&str> = range.split('-').collect();
    if parts.len() != 2 {
        return None;
    }
    let start: u64 = parts[0].parse().ok()?;
    // If the end is omitted, use the file size.
    let end: u64 = if let Ok(e) = parts[1].parse::<u64>() {
        e + 1 // HTTP ranges are inclusive; our reading will use an exclusive end.
    } else {
        file_size
    };
    if start >= end || end > file_size {
        return None;
    }
    Some((start, end))
}

/// Reads bytes from `start` up to (but not including) `end` from the file at `path`.
async fn read_file_range(path: &Path, start: u64, end: u64) -> anyhow::Result<Vec<u8>> {
    let mut file = File::open(path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let mut buffer = vec![0; (end - start) as usize];
    file.read_exact(&mut buffer).await?;
    Ok(buffer)
}
