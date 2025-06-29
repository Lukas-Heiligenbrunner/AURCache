use crate::aur::api::{query_aur, try_get_info_by_name};
use rocket::serde::json::Json;

use crate::api::models::authenticated::Authenticated;
use crate::api::models::input::ApiPackage;
use rocket::get;
use rocket::response::status::BadRequest;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(search))]
pub struct AURApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get all todos", body = [ApiPackage]),
    ),
    params(
        ("query", description = "AUR query"),
    )
)]
#[get("/search?<query>")]
pub async fn search(
    query: &str,
    _a: Authenticated,
) -> Result<Json<Vec<ApiPackage>>, BadRequest<String>> {
    if query.len() < 3 {
        return match try_get_info_by_name(query).await {
            Ok(x) => {
                // Iterate over the Option, giving either a single result or an empty list.
                let mapped = x.into_iter().map(ApiPackage::from).collect();
                Ok(Json(mapped))
            }
            Err(e) => Err(BadRequest(e.to_string())),
        };
    }

    match query_aur(query).await {
        Ok(v) => {
            let mapped = v.into_iter().map(ApiPackage::from).collect();
            Ok(Json(mapped))
        }
        Err(e) => Err(BadRequest(e.to_string())),
    }
}
