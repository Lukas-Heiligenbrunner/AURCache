import 'package:flutter/material.dart';

class QuickInfoData {
  const QuickInfoData({
    Key? key,
    required this.color,
    required this.icon,
    required this.title,
    required this.subtitle,
  });

  final Color color;
  final IconData icon;
  final String title, subtitle;
}
