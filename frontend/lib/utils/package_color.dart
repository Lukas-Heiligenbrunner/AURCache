import 'package:flutter/material.dart';

IconData switchSuccessIcon(int status) {
  switch (status) {
    case 0:
      return Icons.watch_later_outlined;
    case 1:
      return Icons.check_circle_outline;
    case 2:
      return Icons.cancel_outlined;
    case 3:
      return Icons.pause_circle_outline;
    case 4:
      return Icons.pause_circle_outline;
    default:
      return Icons.question_mark_outlined;
  }
}

Color switchSuccessColor(int status) {
  switch (status) {
    case 0:
      return const Color(0xFF9D8D00);
    case 1:
      return const Color(0xFF379137);
    case 2:
      return const Color(0xffFF4752).withOpacity(0.8);
    case 3:
      return const Color(0xFF3C82D2);
    case 4:
      return const Color(0xFF777777);
    default:
      return const Color(0xFF9D8D00);
  }
}

String statusLabel(int status) {
  switch (status) {
    case 0:
      return "Building";
    case 1:
      return "Success";
    case 2:
      return "Failed";
    case 3:
      return "Queued";
    case 4:
      return "Waiting for dependencies";
    default:
      return "Unknown";
  }
}
