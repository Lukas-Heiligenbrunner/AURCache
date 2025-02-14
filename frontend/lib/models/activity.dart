class Activity {
  final String? user;
  final String text;
  final DateTime timestamp;

  Activity({required this.user, required this.text, required this.timestamp});

  factory Activity.fromJson(Map<String, dynamic> json) {
    final timestamp =
        DateTime.fromMillisecondsSinceEpoch(json["timestamp"] * 1000);
    return Activity(
        user: json["user"] as String?,
        text: json["text"] as String,
        timestamp: timestamp);
  }
}
