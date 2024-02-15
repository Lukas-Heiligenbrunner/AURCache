use crate::api::list::build_output;
use crate::api::list::okapi_add_operation_for_get_build_;
use crate::api::list::okapi_add_operation_for_stats_;
use crate::api::list::search;
use crate::api::list::{get_build, okapi_add_operation_for_list_builds_};
use crate::api::list::{list_builds, okapi_add_operation_for_search_};
use crate::api::list::{okapi_add_operation_for_build_output_, stats};
use crate::api::package::okapi_add_operation_for_package_add_endpoint_;
use crate::api::package::okapi_add_operation_for_package_del_;
use crate::api::package::okapi_add_operation_for_package_list_;
use crate::api::package::okapi_add_operation_for_package_update_endpoint_;
use crate::api::package::package_add_endpoint;
use crate::api::package::{get_package, package_del, package_list};
use crate::api::package::{okapi_add_operation_for_get_package_, package_update_endpoint};
use crate::api::remove::okapi_add_operation_for_version_del_;
use crate::api::remove::version_del;
use rocket::Route;
use rocket_okapi::openapi_get_routes;

pub fn build_api() -> Vec<Route> {
    openapi_get_routes![
        search,
        package_list,
        package_add_endpoint,
        package_del,
        version_del,
        build_output,
        list_builds,
        stats,
        get_build,
        get_package,
        package_update_endpoint
    ]
}
