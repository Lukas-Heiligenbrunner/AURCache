import 'package:freezed_annotation/freezed_annotation.dart';
part 'activity.g.dart';

@JsonSerializable()
class Activity {
  final String? user;
  final String text;
  @JsonKey(fromJson: _fromJson)
  final DateTime timestamp;

  Activity({required this.user, required this.text, required this.timestamp});

  factory Activity.fromJson(Map<String, dynamic> json) =>
      _$ActivityFromJson(json);
  Map<String, dynamic> toJson() => _$ActivityToJson(this);

  static DateTime _fromJson(int value) =>
      DateTime.fromMillisecondsSinceEpoch(value * 1000);
}
