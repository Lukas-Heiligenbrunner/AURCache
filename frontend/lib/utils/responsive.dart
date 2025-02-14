import 'package:flutter/material.dart';

extension ResponsiveExt on BuildContext {
  bool isMobile() {
    return mobile;
  }

  bool isDesktop() {
    return desktop;
  }

  bool get mobile => MediaQuery.sizeOf(this).width < 700;
  bool get desktop => !mobile;
}

class Responsive extends StatelessWidget {
  const Responsive(
      {super.key, required this.mobileChild, required this.desktopChild});
  final Widget mobileChild;
  final Widget desktopChild;

  @override
  Widget build(BuildContext context) {
    if (context.mobile) {
      return mobileChild;
    } else {
      return desktopChild;
    }
  }
}

class ResponsiveBuilder extends StatelessWidget {
  const ResponsiveBuilder(
      {super.key, required this.mobile, required this.desktop});
  final Widget Function() mobile;
  final Widget Function() desktop;

  @override
  Widget build(BuildContext context) {
    if (context.mobile) {
      return mobile();
    } else {
      return desktop();
    }
  }
}
