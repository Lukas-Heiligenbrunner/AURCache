//! The build log route, which reads a file rather than a column.
//!
//! Logs moved out of `builds.output` because appending to a row rewrites the
//! whole value; the route now seeks into a file by byte offset. The e2e covers
//! a worker *writing* logs, but on a passing run it never reads one back — the
//! CLI fetch lives in its failure handler — so the read path needs pinning
//! down here.

mod common;

use aurcache_common::build_state::BuildState;
use common::{TestApi, test_api};
use std::convert::identity;

use aurcache_db::builds;
use aurcache_db::packages;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Builds, Packages};
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use sea_orm::ActiveValue::Set;
use sea_orm::{DatabaseConnection, EntityTrait};

/// The log root is read from the environment per call and every test's first
/// build gets id 1, so two tests running at once would share a log file.
///
/// A tokio mutex rather than a std one: the guard is held across awaits, which
/// is what tokio's is for and what clippy rightly refuses for std's.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn test_client(log_root: &std::path::Path) -> TestApi {
    unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", log_root) };
    test_api("test", identity).await
}

/// Seed one package with one build. Logs are keyed by `<pkgbase>/<number>`, so
/// the row id never comes into it.
async fn seed(db: &DatabaseConnection) {
    let pkg_id = Packages::insert(packages::ActiveModel {
        name: Set("hello".to_string()),
        source_data: Set(SourceData::Aur {
            name: "hello".to_string(),
        }),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;

    Builds::insert(builds::ActiveModel {
        number: Set(1),
        pkg_id: Set(pkg_id),
        status: Set(BuildState::Failed),
        platform: Set(Platform::X86_64),
        version: Set("1.0".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap();
}

/// What a request got back.
struct Answer<B> {
    status: Status,
    body: B,
}

async fn get(client: &Client, url: &str) -> Answer<String> {
    let response = client.get(url).dispatch().await;
    Answer {
        status: response.status(),
        body: response.into_string().await.unwrap_or_default(),
    }
}

/// The raw-body variant: the `/output` route answers in bytes, and a test that
/// swims mid-character needs to see them, not a lossy, possibly empty, String.
async fn get_bytes(client: &Client, url: &str) -> Answer<Vec<u8>> {
    let response = client.get(url).dispatch().await;
    Answer {
        status: response.status(),
        body: response.into_bytes().await.unwrap_or_default(),
    }
}

#[rocket::async_test]
async fn reads_the_log_from_a_byte_offset() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let TestApi { client, db } = test_client(root.path()).await;
    seed(&db).await;

    // Named the way the API is: <pkgbase>/<number>.log.
    std::fs::create_dir_all(root.path().join("hello")).unwrap();
    std::fs::write(root.path().join("hello/1.log"), "one\ntwo\n").unwrap();

    let Answer { status, body } = get(&client, "/api/package/hello/build/1/output").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body, "one\ntwo\n");

    // The offset a caller replays is the byte length of what it already has.
    let Answer { status, body } = get(&client, "/api/package/hello/build/1/output?offset=4").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body, "two\n");

    // Caller is already up to date.
    let Answer { body, .. } = get(&client, "/api/package/hello/build/1/output?offset=8").await;
    assert_eq!(body, "");
}

/// A build with no log file is a normal answer, not a 500: it may have produced
/// nothing yet, or its log may have been removed. The UI renders the difference.
#[rocket::async_test]
async fn a_missing_log_is_empty_rather_than_an_error() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let TestApi { client, db } = test_client(root.path()).await;
    seed(&db).await;

    let Answer { status, body } = get(&client, "/api/package/hello/build/1/output").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body, "");
}

/// An unknown build is still a 404. Without the row lookup it would be
/// indistinguishable from a build that has logged nothing.
#[rocket::async_test]
async fn an_unknown_build_is_not_found() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let TestApi { client, db } = test_client(root.path()).await;
    seed(&db).await;

    let Answer { status, .. } = get(&client, "/api/package/hello/build/99/output").await;
    assert_eq!(status, Status::NotFound);
    let Answer { status, .. } = get(&client, "/api/package/nope/build/1/output").await;
    assert_eq!(status, Status::NotFound);
}

/// gcc quotes its diagnostics with multi-byte characters, so an offset landing
/// inside one is reachable in practice. The server answers with the exact raw
/// bytes from the offset — a mid-character cut is a client-side alignment
/// problem, handled by the shared `align` helper, never a server-side guess.
#[rocket::async_test]
async fn an_offset_inside_a_character_returns_raw_bytes() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let TestApi { client, db } = test_client(root.path()).await;
    seed(&db).await;

    std::fs::create_dir_all(root.path().join("hello")).unwrap();

    std::fs::write(root.path().join("hello/1.log"), "option ‘-fno_char8_t’\n").unwrap();

    // "option " is 7 bytes; the quote that follows is 3 (U+2018: e2 80 98).
    // Offset 8 lands on the quote's second byte, which will decode to
    // nothing by itself — but the server does not decode.
    let Answer { status, body } =
        get_bytes(&client, "/api/package/hello/build/1/output?offset=8").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body, b"\x80\x98-fno_char8_t\xE2\x80\x99\n");
    assert_eq!(body.len(), 18, "one page, byte-exact");
}

/// The raw byte slice a mid-character offset produces still aligns cleanly:
/// the shared `align` helper is the API consumer's way back to text, and it
/// must eat exactly the two continuation bytes this test wrote.
#[rocket::async_test]
async fn the_output_aligns_after_a_mid_character_offset() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let TestApi { client, db } = test_client(root.path()).await;
    seed(&db).await;

    std::fs::create_dir_all(root.path().join("hello")).unwrap();

    std::fs::write(root.path().join("hello/1.log"), "option ‘-fno_char8_t’\n").unwrap();

    let Answer { body, .. } =
        get_bytes(&client, "/api/package/hello/build/1/output?offset=8").await;
    let aligned = aurcache_common::api::build_log::align(&body);
    let expected = aurcache_common::api::build_log::Alignment {
        front_skip: 2,
        back_drop: 0,
    };
    assert_eq!(aligned, expected);
    let text = String::from_utf8_lossy(&body[aligned.front_skip..]);
    assert_eq!(text, "-fno_char8_t’\n");
}

/// Cancelling answers for what happened: a queued build is ended, and one that
/// had already finished is refused rather than reported as cancelled.
#[rocket::async_test]
async fn cancel_ends_a_queued_build_and_refuses_a_finished_one() {
    use aurcache_common::build_state::{BuildState, EndReason};
    use sea_orm::{ColumnTrait, QueryFilter};

    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let TestApi { client, db } = test_client(root.path()).await;
    seed(&db).await;

    let url = "/api/package/hello/build/1/cancel";
    let finished = client.post(url).dispatch().await.status();
    assert_eq!(finished, Status::Conflict, "the seeded build has failed");

    Builds::update_many()
        .col_expr(builds::Column::Status, BuildState::Enqueued.into())
        .filter(builds::Column::Number.eq(1))
        .exec(&db)
        .await
        .unwrap();
    assert_eq!(client.post(url).dispatch().await.status(), Status::Ok);
    let build = Builds::find().one(&db).await.unwrap().unwrap();
    assert_eq!(build.status, BuildState::Failed);
    assert_eq!(build.end_reason, Some(EndReason::Canceled));
}
