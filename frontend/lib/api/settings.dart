import 'api_client.dart';

const _unset = Object();

extension SettingsAPI on ApiClient {
  Future<bool> patchSettings({
    int? pkgid,
    Object? auto_update_interval = _unset,
    Object? cpu_limit = _unset,
    Object? memory_limit = _unset,
    Object? max_concurrent_builds = _unset,
    Object? version_check_interval = _unset,
    Object? job_timeout = _unset,
    Object? builder_image = _unset,
  }) async {
    final data = {
      if (auto_update_interval != _unset)
        "auto_update_interval": auto_update_interval,
      if (cpu_limit != _unset) "cpu_limit": cpu_limit,
      if (memory_limit != _unset) "memory_limit": memory_limit,
      if (max_concurrent_builds != _unset)
        "max_concurrent_builds": max_concurrent_builds,
      if (version_check_interval != _unset)
        "version_check_interval": version_check_interval,
      if (job_timeout != _unset) "job_timeout": job_timeout,
      if (builder_image != _unset) "builder_image": builder_image,
    };

    if (pkgid == null) {
      final resp = await getRawClient().patch("/settings", data: data);
      return resp.statusCode == 200;
    } else {
      final resp = await getRawClient().patch("/settings/$pkgid", data: data);
      return resp.statusCode == 200;
    }
  }
}
