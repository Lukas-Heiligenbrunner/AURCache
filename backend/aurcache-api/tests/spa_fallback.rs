//! The frontend is a single-page app, so `/builds` is a route it renders, not
//! a file. These tests pin the server half of that: unknown UI paths get the
//! app shell, while the API keeps its own 404s.
//!
//! Needs the `static` feature, which is what embeds the frontend and mounts the
//! asset handler. Run with `cargo test -p aurcache-api --features static`.

#![cfg(feature = "static")]

mod common;

use aurcache_common::build_state::BuildState;
use common::{TestApi, test_api};

use aurcache_db::packages;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::Packages;
use rocket::http::Status;
use sea_orm::ActiveValue::Set;
use sea_orm::{DatabaseConnection, EntityTrait};

/// Mounts the API *and* the asset handler, in the same order and at the same
/// paths production does. Mount order is the point: the fallback must not
/// shadow the API.
async fn test_client() -> TestApi {
    test_api("test", |rocket| {
        rocket.mount("/", aurcache_api::embed::CustomHandler)
    })
    .await
}

async fn seed(db: &DatabaseConnection, name: &str) -> i32 {
    let model = packages::ActiveModel {
        name: Set(name.to_string()),
        status: Set(BuildState::Active),
        out_of_date: Set(false),
        build_flags: Set(Default::default()),
        platforms: Set("x86_64".parse().unwrap()),
        source_data: Set(SourceData::Aur {
            name: name.to_string(),
        }),
        directly_requested: Set(true),
        ..Default::default()
    };
    Packages::insert(model)
        .exec(db)
        .await
        .expect("insert package")
        .last_insert_id
}

/// The reason the fallback exists. Without it these 404 on reload or when the
/// link is pasted fresh, while working when reached by clicking.
#[rocket::async_test]
async fn frontend_routes_are_served_the_app_shell() {
    let TestApi { client, .. } = test_client().await;

    for path in [
        "/builds",
        "/build/12",
        "/packages",
        "/package/hello",
        "/package/hello/source/PKGBUILD",
        "/settings",
        // Dots in a package name must not be read as a file extension.
        "/package/2048.c",
        "/package/python-3.11",
    ] {
        let response = client.get(path).dispatch().await;
        assert_eq!(
            response.status(),
            Status::Ok,
            "GET {path} should serve the app"
        );
        let body = response.into_string().await.expect("body");
        assert!(
            body.contains("<!doctype html>") || body.contains("<!DOCTYPE html>"),
            "GET {path} did not return the app shell: {body}"
        );
    }
}

/// The risk the fallback introduces: a catch-all that swallows the API. A real
/// API route has to keep winning over it.
#[rocket::async_test]
async fn the_fallback_does_not_shadow_the_api() {
    let TestApi { client, db } = test_client().await;
    seed(&db, "hello").await;

    let response = client.get("/api/package/hello").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let body = response.into_string().await.expect("body");
    assert!(
        body.contains("\"name\":\"hello\""),
        "the API returned something else — the fallback may be shadowing it: {body}"
    );
}

/// An unmatched API path stays a 404. Returning the shell would hand a client
/// HTML where it expects JSON, turning a clear status into a parse error.
#[rocket::async_test]
async fn unmatched_api_paths_still_404() {
    let TestApi { client, .. } = test_client().await;

    for path in ["/api/nope", "/api/package/hello/not-a-thing", "/api"] {
        let response = client.get(path).dispatch().await;
        assert_eq!(
            response.status(),
            Status::NotFound,
            "GET {path} should be a 404, not the app shell"
        );
    }
}

/// A package that does not exist is still a valid *frontend* route: the app
/// loads and renders its own "not found". Only the API knows the difference.
#[rocket::async_test]
async fn an_unknown_package_is_still_a_frontend_route() {
    let TestApi { client, .. } = test_client().await;

    let response = client.get("/package/does-not-exist").dispatch().await;
    assert_eq!(response.status(), Status::Ok);

    let api = client.get("/api/package/does-not-exist").dispatch().await;
    assert_ne!(
        api.status(),
        Status::Ok,
        "the API must still report an unknown package"
    );
}
