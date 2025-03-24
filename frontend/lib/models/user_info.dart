import 'package:freezed_annotation/freezed_annotation.dart';
part 'user_info.g.dart';

@JsonSerializable()
class UserInfo {
  final String? username;

  UserInfo({required this.username});

  factory UserInfo.fromJson(Map<String, dynamic> json) =>
      _$UserInfoFromJson(json);
  Map<String, dynamic> toJson() => _$UserInfoToJson(this);
}
