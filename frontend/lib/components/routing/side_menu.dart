import 'package:flutter/material.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:go_router/go_router.dart';

class SideMenu extends StatelessWidget {
  const SideMenu({
    super.key,
  });

  @override
  Widget build(BuildContext context) {
    return Drawer(
      child: SingleChildScrollView(
        // it enables scrolling
        child: Column(
          children: [
            SizedBox(
              height: 210,
              child: DrawerHeader(
                  child: Column(
                mainAxisAlignment: MainAxisAlignment.start,
                children: [
                  SizedBox(
                    height: 30,
                  ),
                  SvgPicture.asset(
                    'assets/icons/icon.svg',
                    height: 90,
                    width: 00,
                  ),
                  SizedBox(
                    height: 15,
                  ),
                  const Text("AURCache")
                ],
              )),
            ),
            DrawerListTile(
              title: "Dashboard",
              svgSrc: "assets/icons/menu_dashbord.svg",
              press: () {
                context.go("/");
              },
            ),
            DrawerListTile(
              title: "Builds",
              svgSrc: "assets/icons/menu_tran.svg",
              press: () {
                context.go("/builds");
              },
            ),
            DrawerListTile(
              title: "AUR",
              svgSrc: "assets/icons/menu_task.svg",
              press: () {
                context.go("/aur");
              },
            )
          ],
        ),
      ),
    );
  }
}

class DrawerListTile extends StatelessWidget {
  const DrawerListTile({
    super.key,
    required this.title,
    required this.svgSrc,
    required this.press,
  });

  final String title, svgSrc;
  final VoidCallback press;

  @override
  Widget build(BuildContext context) {
    return ListTile(
      onTap: press,
      horizontalTitleGap: 0.0,
      leading: SvgPicture.asset(
        svgSrc,
        color: Colors.white54,
        height: 16,
      ),
      title: Text(
        title,
        style: const TextStyle(color: Colors.white54),
      ),
    );
  }
}
