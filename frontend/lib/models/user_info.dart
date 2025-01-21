class UserInfo {
  final String? username;

  factory UserInfo.fromJson(Map<String, dynamic> json) {
    return UserInfo(
      username: json["username"] as String?,
    );
  }

  UserInfo({required this.username});
}
