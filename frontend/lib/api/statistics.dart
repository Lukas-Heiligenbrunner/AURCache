import '../models/graph_datapoint.dart';
import '../models/stats.dart';
import '../models/user_info.dart';
import 'api_client.dart';

extension StatsAPI on ApiClient {
  Future<Stats> listStats() async {
    final resp = await getRawClient().get("/stats");
    return Stats.fromJson(resp.data);
  }

  Future<UserInfo> userInfo() async {
    final resp = await getRawClient().get("/userinfo");
    return UserInfo.fromJson(resp.data);
  }

  Future<List<GraphDataPoint>> getGraphData() async {
    final resp = await getRawClient().get("/graph");

    final responseObject = resp.data as List;
    final List<GraphDataPoint> packages = responseObject
        .map((e) => GraphDataPoint.fromJson(e))
        .toList(growable: false);
    return packages;
  }
}
