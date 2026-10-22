use rocket::fs::NamedFile;
use rocket::http::uri::Segments;
use rocket::http::{Header, Method, Status};
use rocket::response::Responder;
use rocket::route::{Handler, Outcome};
use rocket::{Data, Request, Response, Route, async_trait, figment};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use tokio::io::AsyncSeekExt;

#[derive(Debug, Clone)]
pub struct CustomFileServer {
    root: PathBuf,
}

impl CustomFileServer {
    /// The rank Rocket's own `FileServer` routes use.
    const RANK: isize = 10;

    #[track_caller]
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            root: path.as_ref().into(),
        }
    }
}

impl From<CustomFileServer> for Vec<Route> {
    fn from(server: CustomFileServer) -> Self {
        let source = figment::Source::File(server.root.clone());
        let mut route = Route::ranked(CustomFileServer::RANK, Method::Get, "/<path..>", server);
        route.name = Some(format!("FileServer: {source}").into());
        vec![route]
    }
}

#[async_trait]
impl Handler for CustomFileServer {
    async fn handle<'r>(&self, req: &'r Request<'_>, data: Data<'r>) -> Outcome<'r> {
        let relative_path = req
            .segments::<Segments<'_, rocket::http::uri::fmt::Path>>(0..)
            .ok()
            // No dot segments: nothing pacman fetches is dot-named, and the
            // repository root also holds uploads being staged for publishing
            // (`.staging`) and databases mid-commit, neither of which is
            // anyone's to download.
            .and_then(|segments| segments.to_path_buf(false).ok());

        // Map uri to filepath
        let Some(relative_path) = relative_path else {
            return Outcome::forward(data, Status::NotFound);
        };
        let file_path = self.root.join(relative_path);

        // open file
        let Ok(named_file) = NamedFile::open(&file_path).await else {
            return Outcome::forward(data, Status::NotFound);
        };

        let metadata = named_file.metadata().await.ok();
        // A directory opens fine but cannot be written as a response body --
        // Rocket fails the write with "Is a directory" (os error 21) and the
        // request answers 500. There is no listing to serve, so a directory
        // is exactly like a missing file.
        if metadata.as_ref().is_some_and(|m| m.file_type().is_dir()) {
            return Outcome::forward(data, Status::NotFound);
        }
        let file_size = metadata.as_ref().map_or(0, std::fs::Metadata::len);
        // HTTP's own date form (`Mon, 06 Oct 2026 10:00:00 GMT`, RFC 9110),
        // which is what `If-Modified-Since` comes back in.
        let last_modified = metadata
            .and_then(|m| m.modified().ok())
            .and_then(|mtime| jiff::Timestamp::try_from(mtime).ok())
            .and_then(|mtime| {
                jiff::fmt::rfc2822::DateTimePrinter::new()
                    .timestamp_to_rfc9110_string(&mtime)
                    .ok()
            });

        // A range starting past the end is 416 with `Content-Range: bytes */size`,
        // which resuming clients (pacman) expect, rather than a silent 200. A
        // header that is not a single byte range is ignored, as HTTP says, and
        // the whole file is sent.
        let range = req
            .headers()
            .get_one("Range")
            .map_or(RangeRequest::Ignored, |h| parse_range_header(h, file_size));
        let mut builder = match range {
            RangeRequest::Ignored => match named_file.respond_to(req) {
                Ok(resp) => Response::build_from(resp),
                Err(_) => return Outcome::error(Status::InternalServerError),
            },
            RangeRequest::Satisfiable(start, end) => {
                // The file already open, not the path again: `repo.db` is
                // renamed over on every commit, and a second open could stream
                // a newer file than the size above was read from.
                let len = usize::try_from(end - start);
                let mut body = named_file.take_file();
                match (len, body.seek(SeekFrom::Start(start)).await) {
                    (Ok(len), Ok(_)) => {
                        // Build a 206 Partial Content response. The builder
                        // methods return `&mut Builder`, so this cannot be a
                        // returned chain.
                        let mut builder = Response::build();
                        builder
                            .status(Status::PartialContent)
                            .raw_header(
                                "Content-Range",
                                format!("bytes {}-{}/{}", start, end - 1, file_size),
                            )
                            .sized_body(len, body);
                        builder
                    }
                    _ => return Outcome::error(Status::InternalServerError),
                }
            }
            RangeRequest::Unsatisfiable => {
                let mut builder = Response::build();
                builder
                    .status(Status::RangeNotSatisfiable)
                    .raw_header("Content-Range", format!("bytes */{file_size}"));
                builder
            }
        };

        // Add Headers
        if let Some(lm) = last_modified {
            builder.header(Header::new("Last-Modified", lm));
        }
        builder.header(Header::new("Accept-Ranges", "bytes"));

        Outcome::Success(builder.finalize())
    }
}

/// What a `Range` header asks of a file of a given size.
#[derive(Debug, PartialEq, Eq)]
enum RangeRequest {
    /// Send `start..end` (exclusive) as 206.
    Satisfiable(u64, u64),
    /// A valid range the file cannot satisfy: 416.
    Unsatisfiable,
    /// Not a single byte range this server understands -- another unit, a
    /// multi-range, malformed syntax. HTTP says to ignore the header and send
    /// the whole file.
    Ignored,
}

/// Parse a `Range` header of the form `bytes=start-end`, `bytes=start-` or
/// `bytes=-suffix` (RFC 9110 §14.1.2).
///
/// An end past the file is clamped rather than refused: a client may ask for
/// more than there is, and gets what there is. Only a start at or past the end
/// (or an empty suffix) is unsatisfiable.
fn parse_range_header(header: &str, file_size: u64) -> RangeRequest {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeRequest::Ignored;
    };
    if spec.contains(',') {
        return RangeRequest::Ignored;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeRequest::Ignored;
    };
    let (first, last) = (first.trim(), last.trim());

    if first.is_empty() {
        // The last `n` bytes.
        return match last.parse::<u64>() {
            Ok(0) => RangeRequest::Unsatisfiable,
            Ok(_) if file_size == 0 => RangeRequest::Unsatisfiable,
            Ok(n) => RangeRequest::Satisfiable(file_size.saturating_sub(n), file_size),
            Err(_) => RangeRequest::Ignored,
        };
    }

    let Ok(start) = first.parse::<u64>() else {
        return RangeRequest::Ignored;
    };
    // Inclusive on the wire, exclusive here; an omitted end is "to the end".
    let end = if last.is_empty() {
        file_size
    } else {
        match last.parse::<u64>() {
            Ok(inclusive) if inclusive < start => return RangeRequest::Ignored,
            Ok(inclusive) => inclusive.saturating_add(1).min(file_size),
            Err(_) => return RangeRequest::Ignored,
        }
    };
    if start >= file_size {
        return RangeRequest::Unsatisfiable;
    }
    RangeRequest::Satisfiable(start, end)
}

#[cfg(test)]
mod download_tests {
    use super::{RangeRequest, parse_range_header};

    /// The parser speaks exclusive ends: HTTP ranges are inclusive, and an
    /// omitted end means "to the end of the file".
    #[test]
    fn range_bounds_are_exclusive_with_open_end() {
        use RangeRequest::Satisfiable;
        assert_eq!(parse_range_header("bytes=0-99", 1000), Satisfiable(0, 100));
        assert_eq!(
            parse_range_header("bytes=100-", 1000),
            Satisfiable(100, 1000)
        );
        assert_eq!(
            parse_range_header("bytes=500-500", 1000),
            Satisfiable(500, 501)
        );
    }

    /// Asking for more than there is gets what there is, and a suffix is the
    /// last bytes of the file -- neither is an error.
    #[test]
    fn long_and_suffix_ranges_are_satisfied() {
        use RangeRequest::Satisfiable;
        assert_eq!(
            parse_range_header("bytes=0-9999", 1000),
            Satisfiable(0, 1000)
        );
        assert_eq!(
            parse_range_header("bytes=-100", 1000),
            Satisfiable(900, 1000)
        );
        assert_eq!(
            parse_range_header("bytes=-5000", 1000),
            Satisfiable(0, 1000)
        );
    }

    /// Only a range starting past the end is answered 416.
    #[test]
    fn a_start_past_the_end_is_unsatisfiable() {
        use RangeRequest::Unsatisfiable;
        assert_eq!(parse_range_header("bytes=1000-", 1000), Unsatisfiable);
        assert_eq!(parse_range_header("bytes=9999-", 1000), Unsatisfiable);
        assert_eq!(parse_range_header("bytes=-0", 1000), Unsatisfiable);
        assert_eq!(parse_range_header("bytes=-10", 0), Unsatisfiable);
    }

    /// What is not a single byte range is ignored, so the whole file is sent.
    #[test]
    fn other_ranges_are_ignored() {
        use RangeRequest::Ignored;
        assert_eq!(parse_range_header("bytes=600-100", 1000), Ignored);
        assert_eq!(parse_range_header("items=0-99", 1000), Ignored);
        assert_eq!(parse_range_header("bytes=abc-def", 1000), Ignored);
        assert_eq!(parse_range_header("bytes=0-abc", 1000), Ignored);
        assert_eq!(parse_range_header("bytes=0-9,20-29", 1000), Ignored);
    }
}

#[cfg(test)]
mod serve_tests {
    use super::CustomFileServer;
    use rocket::http::Status;
    use rocket::local::asynchronous::Client;

    async fn served_client(root: &std::path::Path) -> Client {
        Client::tracked(rocket::build().mount("/", CustomFileServer::new(root)))
            .await
            .expect("tracked client builds")
    }

    /// A directory is not a file to download: opening one used to succeed and
    /// the response write then failed with "Is a directory" (os error 21),
    /// answering 500. There is no listing to serve, so a directory answers
    /// exactly like a missing file.
    #[rocket::async_test]
    async fn directories_answer_like_missing_files() {
        let root = tempfile::tempdir().unwrap();
        let arch = root.path().join("x86_64");
        std::fs::create_dir(&arch).unwrap();
        std::fs::write(arch.join("repo.db"), b"db").unwrap();

        let client = served_client(root.path()).await;

        let file = client.get("/x86_64/repo.db").dispatch().await;
        assert_eq!(file.status(), Status::Ok);

        for path in ["/x86_64", "/x86_64/", "/"] {
            let response = client.get(path).dispatch().await;
            assert_eq!(response.status(), Status::NotFound, "GET {path}");
        }

        let missing = client.get("/x86_64/no-such-file").dispatch().await;
        assert_eq!(missing.status(), Status::NotFound);
    }
}
