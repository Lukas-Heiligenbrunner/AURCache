use crate::aur::search;
use crate::auth::{regenerate_token, user_info};
use crate::build::{
    build_output, cancel_build, delete_build, download_build_output, get_build, list_builds,
    list_package_builds, retry_build,
};
use crate::health::{health, server_info};
use crate::log::log;
use crate::package::{
    active_operations, add_package, add_packages, bulk_add_progress, delete_package,
    dependency_options, get_package, get_source_file, list_packages, list_source_files,
    patch_package, preview_source_file, preview_source_files, put_source_file, replace_dependency,
    update_package,
};
use crate::settings::{
    get_package_setting, get_package_settings, get_setting, get_settings, patch_package_setting,
    patch_setting, preview_schedule, reset_package_setting, reset_setting,
};
use crate::stats::{dashboard, graph, stats};
use rocket::{Route, routes};

#[must_use]
pub fn build_api() -> Vec<Route> {
    routes![
        search,
        regenerate_token,
        list_packages,
        crate::dump::dump,
        crate::dump::restore,
        crate::dump::restore_progress,
        add_package,
        add_packages,
        active_operations,
        bulk_add_progress,
        delete_package,
        dependency_options,
        replace_dependency,
        patch_package,
        build_output,
        download_build_output,
        delete_build,
        list_builds,
        list_package_builds,
        stats,
        dashboard,
        graph,
        user_info,
        get_build,
        get_package,
        retry_build,
        update_package,
        list_source_files,
        get_source_file,
        put_source_file,
        preview_source_files,
        preview_source_file,
        cancel_build,
        health,
        server_info,
        crate::repo::repo_info,
        log,
        get_settings,
        get_package_settings,
        get_package_setting,
        patch_package_setting,
        reset_package_setting,
        get_setting,
        patch_setting,
        reset_setting,
        preview_schedule
    ]
}
