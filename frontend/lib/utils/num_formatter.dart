extension ShortNrFormatter on int {
  String shortForm() {
    if (this < 1000) {
      return toString();
    } else if (this < 1000000) {
      return "${(this / 1000).floor()}k";
    } else {
      return "${(this / 1000000).floor()}m";
    }
  }
}
