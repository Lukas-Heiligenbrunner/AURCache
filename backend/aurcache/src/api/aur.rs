use crate::aur::api::{get_info_by_name, query_aur};
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
        return match get_info_by_name(query).await {
            Ok(x) => {
                let mapped = vec![ApiPackage::from(x)];
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
