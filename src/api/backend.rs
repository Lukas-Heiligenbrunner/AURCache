use crate::api::add::okapi_add_operation_for_package_add_;
use crate::api::add::package_add;
use crate::api::list::okapi_add_operation_for_build_output_;
use crate::api::list::okapi_add_operation_for_list_builds_;
use crate::api::list::{build_output, okapi_add_operation_for_package_list_};
use crate::api::list::{list_builds, okapi_add_operation_for_search_};
use crate::api::list::{package_list, search};
use crate::api::remove::okapi_add_operation_for_package_del_;
use crate::api::remove::okapi_add_operation_for_version_del_;
use crate::api::remove::{package_del, version_del};
use rocket::Route;
use rocket_okapi::openapi_get_routes;

pub fn build_api() -> Vec<Route> {
    openapi_get_routes![
        search,
        package_list,
        package_add,
        package_del,
        version_del,
        build_output,
        list_builds
    ]
}
