use crate::models::authenticated::Authenticated;
use crate::models::input::ApiPackage;
use aurcache_utils::aur::api::{get_package_info, query_aur};
use rocket::get;
use rocket::response::status::BadRequest;
use rocket::serde::json::Json;
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
        return match get_package_info(query).await {
            Ok(x) => {
                // Iterate over the Option, giving either a single result or an empty list.
                let pkg = x.into_iter().map(ApiPackage::from).collect();
                Ok(Json(pkg))
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
