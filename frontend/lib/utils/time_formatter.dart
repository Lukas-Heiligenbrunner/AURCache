String _s(num n) => n == 1 ? '' : 's';

extension TimeFormatter on DateTime {
  String readableDuration() {
    final now = DateTime.now();
    final duration = now.difference(this);

    if (duration.inSeconds < 60) {
      return '${duration.inSeconds} Second${_s(duration.inSeconds)} ago';
    } else if (duration.inMinutes < 60) {
      return '${duration.inMinutes} Minute${_s(duration.inMinutes)})} ago';
    } else if (duration.inHours < 24) {
      return '${duration.inHours} Hour${_s(duration.inHours)} ago';
    } else if (duration.inDays < 30) {
      return '${duration.inDays} Day${_s(duration.inDays)} ago';
    } else if ((duration.inDays / 30) < 12) {
      return '${duration.inDays ~/ 30} Month${_s(duration.inDays ~/ 30)} ago';
    } else {
      return '${duration.inDays ~/ 365} Year${_s(duration.inDays ~/ 365)} ago';
    }
  }
}

extension DurationFormatter on Duration {
  String readableDuration() {
    if (inSeconds < 60) {
      return '$inSeconds Second${_s(inSeconds)}';
    } else if (inMinutes < 60) {
      return '$inMinutes Minute${_s(inMinutes)}';
    } else if (inHours < 24) {
      return '$inHours Hour${_s(inHours)}';
    } else if (inDays < 30) {
      return '$inDays Day${_s(inDays)}';
    } else if ((inDays / 30) < 12) {
      return '${inDays ~/ 30} Month${_s(inDays ~/ 30)}';
    } else {
      return '${inDays ~/ 365} Year${_s(inDays ~/ 365)}';
    }
  }
}
