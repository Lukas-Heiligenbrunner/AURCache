import '../models/stats.dart';
import 'api_client.dart';

extension StatsAPI on ApiClient {
  Future<Stats> listStats() async {
    final resp = await getRawClient().get("/stats");
    return Stats.fromJson(resp.data);
  }
}
