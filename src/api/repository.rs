use rocket::fs::FileServer;

pub fn build_api() -> FileServer {
    FileServer::from("./repo")
}
