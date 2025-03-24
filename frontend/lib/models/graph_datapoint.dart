import 'package:freezed_annotation/freezed_annotation.dart';
part 'graph_datapoint.g.dart';

@JsonSerializable()
class GraphDataPoint {
  final int month, year, count;

  GraphDataPoint({
    required this.month,
    required this.year,
    required this.count,
  });

  factory GraphDataPoint.fromJson(Map<String, dynamic> json) =>
      _$GraphDataPointFromJson(json);
  Map<String, dynamic> toJson() => _$GraphDataPointToJson(this);
}
