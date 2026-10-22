use crate::models::aur::ApiPackage;
use crate::models::authenticated::Authenticated;
use crate::utils::error::{ApiError, err};
use aurcache_utils::aur::api::{get_package_info, query_aur};
use aurcache_utils::services::Services;
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(search))]
pub struct AURApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Matching AUR packages", body = [ApiPackage]),
            (status = 502, description = "The AUR could not be asked"),
    ),
    params(
        ("query", description = "AUR query"),
    )
)]
/// Search the AUR, treating short queries as exact package names.
#[get("/search?<query>")]
pub async fn search(
    query: &str,
    services: &State<Services>,
    _a: Authenticated,
) -> Result<Json<Vec<ApiPackage>>, ApiError> {
    // Chars, not bytes: a byte length miscounts non-ASCII queries against the
    // minimum the info endpoint needs. One shared tail, so the two branches
    // cannot drift apart again.
    let result = if query.chars().count() < 3 {
        // Iterate over the Option, giving either a single result or an empty list.
        get_package_info(&services.client, query)
            .await
            .map(|pkg| pkg.into_iter().collect::<Vec<_>>())
    } else {
        query_aur(&services.client, query).await
    };
    // The AUR's failure, not the request's.
    let packages = result.map_err(|e| err(Status::BadGateway, e))?;
    Ok(Json(packages.into_iter().map(ApiPackage::from).collect()))
}
