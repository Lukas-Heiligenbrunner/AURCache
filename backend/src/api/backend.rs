use crate::api::aur::*;
use crate::api::build::*;
use crate::api::health::*;
use crate::api::package::*;
use crate::api::stats::*;
use rocket::Route;
use rocket_okapi::openapi_get_routes;

pub fn build_api() -> Vec<Route> {
    openapi_get_routes![
        search,
        package_list,
        package_add_endpoint,
        package_del,
        package_update_entity_endpoint,
        build_output,
        delete_build,
        list_builds,
        stats,
        get_build,
        get_package,
        rery_build,
        package_update_endpoint,
        cancel_build,
        health
    ]
}
