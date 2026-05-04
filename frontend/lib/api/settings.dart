import '../models/settings.dart';
import 'api_client.dart';

extension SettingsAPI on ApiClient {
  /// Update a single setting by key. The backend stores everything as text;
  /// callers must convert numbers/cron strings/etc. to a string first.
  Future<bool> patchSetting(String key, String value, {int? pkgid}) async {
    final query = pkgid == null ? '' : '?pkgid=$pkgid';
    final resp = await getRawClient().patch(
      "/settings/$key$query",
      data: {"value": value},
    );
    return resp.statusCode == 200;
  }

  /// Reset a single setting back to its default by deleting any stored
  /// override.
  Future<bool> resetSetting(String key, {int? pkgid}) async {
    final query = pkgid == null ? '' : '?pkgid=$pkgid';
    final resp = await getRawClient().delete("/settings/$key$query");
    return resp.statusCode == 200;
  }

  /// Fetch a single setting (used for large blobs like makepkg.conf /
  /// pacman.conf that are not part of the bulk dashboard payload).
  Future<SingleSetting> getSetting(String key, {int? pkgid}) async {
    final query = pkgid == null ? null : {'pkgid': pkgid};
    final resp = await getRawClient().get(
      "/settings/$key",
      queryParameters: query,
    );
    return SingleSetting.fromJson(resp.data as Map<String, dynamic>);
  }
}
