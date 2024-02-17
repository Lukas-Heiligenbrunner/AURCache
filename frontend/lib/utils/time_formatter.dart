extension TimeFormatter on DateTime {
  String readableDuration() {
    final now = DateTime.now();
    final duration = now.difference(this);

    if (duration.inSeconds < 60) {
      return '${duration.inSeconds} seconds ago';
    } else if (duration.inMinutes < 60) {
      return '${duration.inMinutes} minutes ago';
    } else if (duration.inHours < 24) {
      return '${duration.inHours} hours ago';
    } else if (duration.inDays < 30) {
      return '${duration.inDays} days ago';
    } else if ((duration.inDays / 30) < 12) {
      return '${duration.inDays ~/ 30} months ago';
    } else {
      return '${duration.inDays ~/ 365} years ago';
    }
  }
}

extension DurationFormatter on Duration {
  String readableDuration() {
    if (inSeconds < 60) {
      return '$inSeconds second${inSeconds != 1 ? 's' : ''}';
    } else if (inMinutes < 60) {
      return '$inMinutes minute${inMinutes != 1 ? 's' : ''}';
    } else if (inHours < 24) {
      return '$inHours hour${inHours != 1 ? 's' : ''}';
    } else if (inDays < 30) {
      return '$inDays day${inDays != 1 ? 's' : ''}';
    } else if ((inDays / 30) < 12) {
      return '${inDays ~/ 30} month${(inDays ~/ 30) != 1 ? 's' : ''}';
    } else {
      return '${inDays ~/ 365} year${(inDays ~/ 365) != 1 ? 's' : ''}';
    }
  }
}
