use crate::api::aur::*;
use crate::api::build::*;
use crate::api::package::*;
use crate::api::stats::*;
use crate::api::version::*;
use rocket::Route;
use rocket_okapi::openapi_get_routes;

pub fn build_api() -> Vec<Route> {
    openapi_get_routes![
        search,
        package_list,
        package_add_endpoint,
        package_del,
        build_output,
        delete_build,
        list_builds,
        stats,
        get_build,
        get_package,
        package_update_endpoint,
        cancel_build
    ]
}
