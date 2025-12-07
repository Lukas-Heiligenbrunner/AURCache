use crate::activity::activity;
use crate::aur::search;
use crate::build::{build_output, cancel_build, delete_build, get_build, list_builds, rery_build};
use crate::health::health;
use crate::package::{
    get_package, package_add_endpoint, package_del, package_list, package_update_endpoint,
    package_update_entity_endpoint,
};
use crate::stats::{dashboard_graph_data, stats, user_info};
use rocket::{Route, routes};
use crate::settings::{setting_update, settings};

#[must_use]
pub fn build_api() -> Vec<Route> {
    routes![
        search,
        package_list,
        package_add_endpoint,
        package_del,
        package_update_entity_endpoint,
        build_output,
        delete_build,
        list_builds,
        stats,
        dashboard_graph_data,
        user_info,
        get_build,
        get_package,
        rery_build,
        package_update_endpoint,
        cancel_build,
        health,
        activity,
        settings,
        setting_update
    ]
}
