use crate::aur::aur::query_aur;
use rocket::serde::json::Json;

use rocket::get;

use crate::api::types::input::ApiPackage;
use rocket_okapi::openapi;

#[openapi(tag = "aur")]
#[get("/search?<query>")]
pub async fn search(query: &str) -> Result<Json<Vec<ApiPackage>>, String> {
    return match query_aur(query).await {
        Ok(v) => {
            let mapped = v
                .iter()
                .map(|x| ApiPackage {
                    name: x.name.clone(),
                    version: x.version.clone(),
                })
                .collect();
            Ok(Json(mapped))
        }
        Err(e) => Err(format!("{}", e)),
    };
}
