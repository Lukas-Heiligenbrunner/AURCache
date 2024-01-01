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
