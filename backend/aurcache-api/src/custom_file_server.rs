use rocket::fs::NamedFile;
use rocket::http::uri::Segments;
use rocket::http::{Header, Method, RawStr, Status};
use rocket::response::{Redirect, Responder, content::RawHtml};
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

    /// A plain index, using the same no-dot-names policy as downloads.
    async fn directory_index(&self, path: &Path, is_root: bool) -> std::io::Result<String> {
        let mut entries = Vec::new();
        let mut directory = tokio::fs::read_dir(path).await?;
        while let Some(entry) = directory.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            // Follow symlinks, as downloads do: repo.db and repo.files can
            // be links. A file may disappear during a repository update.
            let Ok(metadata) = tokio::fs::metadata(entry.path()).await else {
                continue;
            };
            if metadata.is_file() || metadata.is_dir() {
                entries.push((name.to_owned(), metadata.is_dir()));
            }
        }
        entries.sort_by(|(a, a_dir), (b, b_dir)| b_dir.cmp(a_dir).then_with(|| a.cmp(b)));

        let mut html = String::from(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\n\
             <title>Repository index</title></head><body><h1>Repository index</h1><ul>\n",
        );
        if !is_root {
            html.push_str("<li><a href=\"../\">Parent directory</a></li>\n");
        }
        for (name, is_dir) in entries {
            let name = RawStr::new(&name);
            let href = name.percent_encode();
            let label = name.html_escape();
            let suffix = if is_dir { "/" } else { "" };
            html.push_str(&format!(
                "<li><a href=\"./{href}{suffix}\">{label}{suffix}</a></li>\n"
            ));
        }
        html.push_str("</ul></body></html>\n");
        Ok(html)
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
        let file_path = self.root.join(&relative_path);

        // open file
        let Ok(named_file) = NamedFile::open(&file_path).await else {
            return Outcome::forward(data, Status::NotFound);
        };

        let metadata = named_file.metadata().await.ok();
        if metadata.as_ref().is_some_and(std::fs::Metadata::is_dir) {
            // Relative entry and parent links need a directory URL, even
            // when the repository is mounted under a reverse proxy prefix.
            if !req.uri().path().ends_with('/') {
                let uri = req.uri().clone().into_normalized();
                return Outcome::from(req, Redirect::permanent(format!("{}/", uri.path())));
            }
            return match self
                .directory_index(&file_path, relative_path.as_os_str().is_empty())
                .await
            {
                Ok(html) => Outcome::from(req, RawHtml(html)),
                Err(_) => Outcome::error(Status::InternalServerError),
            };
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
            RangeRequest::Satisfiable { start, end } => {
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
    /// Send `start..end` (`end` exclusive) as 206.
    Satisfiable { start: u64, end: u64 },
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
            Ok(n) => RangeRequest::Satisfiable {
                start: file_size.saturating_sub(n),
                end: file_size,
            },
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
    RangeRequest::Satisfiable { start, end }
}

#[cfg(test)]
mod download_tests {
    use super::{RangeRequest, parse_range_header};

    /// The parser speaks exclusive ends: HTTP ranges are inclusive, and an
    /// omitted end means "to the end of the file".
    #[test]
    fn range_bounds_are_exclusive_with_open_end() {
        use RangeRequest::Satisfiable;
        assert_eq!(
            parse_range_header("bytes=0-99", 1000),
            Satisfiable { start: 0, end: 100 }
        );
        assert_eq!(
            parse_range_header("bytes=100-", 1000),
            Satisfiable {
                start: 100,
                end: 1000
            }
        );
        assert_eq!(
            parse_range_header("bytes=500-500", 1000),
            Satisfiable {
                start: 500,
                end: 501
            }
        );
    }

    /// Asking for more than there is gets what there is, and a suffix is the
    /// last bytes of the file -- neither is an error.
    #[test]
    fn long_and_suffix_ranges_are_satisfied() {
        use RangeRequest::Satisfiable;
        assert_eq!(
            parse_range_header("bytes=0-9999", 1000),
            Satisfiable {
                start: 0,
                end: 1000
            }
        );
        assert_eq!(
            parse_range_header("bytes=-100", 1000),
            Satisfiable {
                start: 900,
                end: 1000
            }
        );
        assert_eq!(
            parse_range_header("bytes=-5000", 1000),
            Satisfiable {
                start: 0,
                end: 1000
            }
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

    #[rocket::async_test]
    async fn directories_are_browsable_and_files_still_download() {
        let root = tempfile::tempdir().unwrap();
        let arch = root.path().join("x86_64");
        std::fs::create_dir(&arch).unwrap();
        std::fs::write(arch.join("repo.db"), b"db").unwrap();
        std::fs::write(arch.join("a.pkg.tar.zst"), b"package").unwrap();
        std::fs::create_dir(arch.join("nested")).unwrap();
        let client = served_client(root.path()).await;

        let root_page = client
            .get("/")
            .dispatch()
            .await
            .into_string()
            .await
            .unwrap();
        assert!(root_page.contains("href=\"./x86_64/\""));
        assert!(!root_page.contains("Parent directory"));
        let redirect = client.get("/x86_64").dispatch().await;
        assert_eq!(redirect.status(), Status::PermanentRedirect);
        assert_eq!(redirect.headers().get_one("Location"), Some("/x86_64/"));
        let page = client.get("/x86_64/").dispatch().await;
        assert_eq!(page.status(), Status::Ok);
        assert_eq!(page.content_type(), Some(rocket::http::ContentType::HTML));
        let html = page.into_string().await.unwrap();
        assert!(html.contains("href=\"../\""));
        assert!(html.contains("href=\"./repo.db\""));
        assert!(html.find("./nested/").unwrap() < html.find("./a.pkg.tar.zst").unwrap());
        assert!(html.find("./a.pkg.tar.zst").unwrap() < html.find("./repo.db").unwrap());
        let file = client.get("/x86_64/repo.db").dispatch().await;
        assert_eq!(file.status(), Status::Ok);
        assert_eq!(file.into_bytes().await.unwrap(), b"db");
        assert_eq!(
            client.get("/missing").dispatch().await.status(),
            Status::NotFound
        );
        // Empty directories also have a valid index.
        assert_eq!(
            client.get("/x86_64/nested/").dispatch().await.status(),
            Status::Ok
        );
    }

    #[rocket::async_test]
    async fn dot_files_and_directories_are_neither_listed_nor_downloadable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".staging")).unwrap();
        std::fs::create_dir(root.path().join("x86_64")).unwrap();
        for path in [
            ".staging/private.pkg.tar.zst",
            ".repo.db.tmp",
            "x86_64/.repo.files.tmp",
        ] {
            std::fs::write(root.path().join(path), b"private").unwrap();
        }
        let client = served_client(root.path()).await;
        for path in ["/", "/x86_64/"] {
            let html = client
                .get(path)
                .dispatch()
                .await
                .into_string()
                .await
                .unwrap();
            assert!(!html.contains(".staging"));
            assert!(!html.contains(".tmp"));
        }
        for path in [
            "/.staging/",
            "/.staging/private.pkg.tar.zst",
            "/.repo.db.tmp",
            "/x86_64/.repo.files.tmp",
            "/%2Estaging/",
            "/x86_64/%2Erepo.files.tmp",
        ] {
            assert_eq!(
                client.get(path).dispatch().await.status(),
                Status::NotFound,
                "{path}"
            );
        }
    }

    #[rocket::async_test]
    async fn listing_escapes_html_and_links_round_trip_special_filenames() {
        let root = tempfile::tempdir().unwrap();
        let name = "<tag> & \"quote\" #?% café.pkg.tar.zst";
        std::fs::write(root.path().join(name), b"package").unwrap();
        let client = served_client(root.path()).await;
        let html = client
            .get("/")
            .dispatch()
            .await
            .into_string()
            .await
            .unwrap();
        assert!(!html.contains("<tag>"));
        assert!(html.contains("&lt;tag&gt;"));
        assert!(html.contains("&amp;"));
        // Follow the actual generated URL, including its percent encoding.
        let href = html
            .split("href=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let response = client
            .get(format!("/{}", href.strip_prefix("./").unwrap()))
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_bytes().await.unwrap(), b"package");
    }

    #[rocket::async_test]
    async fn directory_redirects_never_become_network_path_references() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("x86_64")).unwrap();
        let client = served_client(root.path()).await;
        for path in ["//x86_64", "///x86_64", "/x86_64//nested/.."] {
            let response = client.get(path).dispatch().await;
            assert_eq!(response.status(), Status::PermanentRedirect, "{path}");
            let location = response.headers().get_one("Location").unwrap();
            assert!(location.starts_with('/'));
            assert!(!location.starts_with("//"), "{path}: {location}");
        }
    }

    #[rocket::async_test]
    async fn listing_and_redirects_work_at_a_mount_prefix() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("x86_64")).unwrap();
        std::fs::write(root.path().join("x86_64/repo.db"), b"db").unwrap();
        let client =
            Client::tracked(rocket::build().mount("/repo", CustomFileServer::new(root.path())))
                .await
                .unwrap();
        let redirect = client.get("/repo/x86_64").dispatch().await;
        assert_eq!(
            redirect.headers().get_one("Location"),
            Some("/repo/x86_64/")
        );
        let html = client
            .get("/repo/x86_64/")
            .dispatch()
            .await
            .into_string()
            .await
            .unwrap();
        assert!(html.contains("href=\"./repo.db\""));
        assert!(html.contains("href=\"../\""));
    }

    #[cfg(unix)]
    #[rocket::async_test]
    async fn repository_database_symlinks_are_listed_and_downloadable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("repo.db.tar.gz"), b"db").unwrap();
        std::os::unix::fs::symlink("repo.db.tar.gz", root.path().join("repo.db")).unwrap();
        let client = served_client(root.path()).await;
        let html = client
            .get("/")
            .dispatch()
            .await
            .into_string()
            .await
            .unwrap();
        assert!(html.contains("href=\"./repo.db\""));
        let response = client.get("/repo.db").dispatch().await;
        assert_eq!(response.into_bytes().await.unwrap(), b"db");
    }
}
