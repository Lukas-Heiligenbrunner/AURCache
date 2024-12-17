import 'package:flutter/material.dart';

import '../../utils/responsive.dart';
import 'side_menu.dart';

class MenuShell extends StatelessWidget {
  const MenuShell({super.key, required this.child});
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      drawer: const SideMenu(),
      body: SafeArea(
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            // We want this side menu only for large screen
            if (context.desktop)
              const Expanded(
                // default flex = 1
                // and it takes 1/6 part of the screen
                child: SideMenu(),
              ),
            Expanded(
              // It takes 5/6 part of the screen
              flex: 7,
              child: child,
            ),
          ],
        ),
      ),
    );
  }
}
