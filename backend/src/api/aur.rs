use crate::aur::api::query_aur;
use rocket::serde::json::Json;

use rocket::get;

use crate::api::types::authenticated::Authenticated;
use crate::api::types::input::ApiPackage;
use rocket_okapi::openapi;

#[openapi(tag = "aur")]
#[get("/search?<query>")]
pub async fn search(query: &str, _a: Authenticated) -> Result<Json<Vec<ApiPackage>>, String> {
    if query.len() < 2 {
        return Err("Query too short".to_string());
    }

    match query_aur(query).await {
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
    }
}
