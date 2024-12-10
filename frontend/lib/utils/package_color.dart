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
      return Icons.remove_circle_outline;
    default:
      return Icons.question_mark_outlined;
  }
}

Color switchSuccessColor(int status) {
  switch (status) {
    case 0:
      return const Color(0xFF9D8D00);
    case 1:
      return const Color(0xFF0A6900);
    case 4:
    case 2:
      return const Color(0xffFF4752).withOpacity(0.8);
    case 3:
      return const Color(0xFF0044AA);
    default:
      return const Color(0xFF9D8D00);
  }
}
