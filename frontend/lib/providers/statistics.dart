import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';

import '../api/API.dart';
import '../api/api_client.dart';
import '../models/graph_datapoint.dart';
import '../models/stats.dart';
import '../models/user_info.dart';

part 'statistics.g.dart';

@riverpod
Future<Stats> listStats(Ref ref) async {
  final resp = await API.getRawClient().get("/stats");
  return Stats.fromJson(resp.data);
}

@riverpod
Future<UserInfo> userInfo(Ref ref) async {
  final resp = await API.getRawClient().get("/userinfo");
  return UserInfo.fromJson(resp.data);
}

@riverpod
Future<List<GraphDataPoint>> getGraphData(Ref ref) async {
  final resp = await API.getRawClient().get("/graph");

  final responseObject = resp.data as List;
  final List<GraphDataPoint> packages = responseObject
      .map((e) => GraphDataPoint.fromJson(e))
      .toList(growable: false);
  return packages;
}
