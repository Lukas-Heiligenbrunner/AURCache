use crate::aur::search;
use crate::auth::regenerate_api_token_endpoint;
use crate::build::{
    build_output, cancel_build, delete_build, download_build_output, get_build, list_builds,
    list_package_builds, retry_build,
};
use crate::health::{health, server_version};
use crate::log::log;
use crate::package::{
    active_operations, bulk_add_progress, get_package, package_add_endpoint, package_del,
    package_dependency_options, package_dependency_replace, package_list, package_source_file,
    package_source_file_update, package_source_files, package_source_preview_file,
    package_source_preview_files, package_update_endpoint, package_update_entity_endpoint,
    packages_add_endpoint,
};
use crate::settings::{
    package_setting_get, package_setting_patch, package_setting_reset, package_settings,
    setting_get, setting_patch, setting_reset, setting_schedule_preview, settings,
};
use crate::stats::{dashboard, dashboard_graph_data, stats, user_info};
use rocket::{Route, routes};

#[must_use]
pub fn build_api() -> Vec<Route> {
    routes![
        search,
        regenerate_api_token_endpoint,
        package_list,
        crate::dump::dump,
        crate::dump::restore,
        crate::dump::restore_progress,
        package_add_endpoint,
        packages_add_endpoint,
        active_operations,
        bulk_add_progress,
        package_del,
        package_dependency_options,
        package_dependency_replace,
        package_update_entity_endpoint,
        build_output,
        download_build_output,
        delete_build,
        list_builds,
        list_package_builds,
        stats,
        dashboard,
        dashboard_graph_data,
        user_info,
        get_build,
        get_package,
        retry_build,
        package_update_endpoint,
        package_source_files,
        package_source_file,
        package_source_file_update,
        package_source_preview_files,
        package_source_preview_file,
        cancel_build,
        health,
        server_version,
        crate::repo::repo_info,
        log,
        settings,
        package_settings,
        package_setting_get,
        package_setting_patch,
        package_setting_reset,
        setting_get,
        setting_patch,
        setting_reset,
        setting_schedule_preview
    ]
}
