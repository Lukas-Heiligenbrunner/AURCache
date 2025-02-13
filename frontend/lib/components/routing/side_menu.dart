import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:go_router/go_router.dart';

class SideMenu extends StatelessWidget {
  const SideMenu({
    super.key,
  });

  @override
  Widget build(BuildContext context) {
    final activeUri = GoRouterState.of(context).uri.toString();

    return Drawer(
      //backgroundColor: Color(0xff131418),
      child: SingleChildScrollView(
        child: Column(
          children: [
            SizedBox(
              height: 30,
            ),
            Row(
              mainAxisAlignment: MainAxisAlignment.spaceBetween,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Padding(
                  padding: const EdgeInsets.only(left: 20),
                  child: SvgPicture.asset(
                    'assets/icons/icon.svg',
                    height: 55,
                  ),
                ),
                Padding(
                  padding: const EdgeInsets.only(right: 20),
                  child: Column(
                    mainAxisAlignment: MainAxisAlignment.start,
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      const Text(
                        "AURCache",
                        style: TextStyle(
                            fontWeight: FontWeight.w900, fontSize: 16),
                      ),
                      const Text(
                        "The Archlinux AUR\nbuild server",
                        style: TextStyle(fontSize: 12),
                      )
                    ],
                  ),
                )
              ],
            ),
            SizedBox(
              height: 20,
            ),
            DrawerSection(title: "General", children: [
              DrawerListTile(
                title: "Dashboard",
                svgSrc: "assets/icons/menu/dashboard.svg",
                active: activeUri == "/",
                press: () {
                  context.go("/");
                },
              ),
              DrawerListTile(
                title: "Builds",
                svgSrc: "assets/icons/menu/builds.svg",
                active: activeUri.startsWith("/builds"),
                press: () {
                  context.go("/builds");
                },
              ),
              DrawerListTile(
                title: "AUR",
                svgSrc: "assets/icons/menu/aur.svg",
                active: activeUri.startsWith("/aur"),
                press: () {
                  context.go("/aur");
                },
              )
            ]),
            SizedBox(
              height: 40,
            ),
            DrawerSection(title: "Settings", children: [
              DrawerListTile(
                title: "Settings",
                svgSrc: "assets/icons/menu/settings.svg",
                active: activeUri.startsWith("/settings"),
                press: () {
                  context.go("/settings");
                },
              ),
              DrawerListTile(
                title: "Help",
                svgSrc: "assets/icons/menu/help.svg",
                active: activeUri.startsWith("/help"),
                press: () {
                  context.go("/help");
                },
              ),
            ]),
          ],
        ),
      ),
    );
  }
}

class DrawerSection extends StatelessWidget {
  const DrawerSection({super.key, required this.title, required this.children});

  final String title;
  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    return Column(
      mainAxisAlignment: MainAxisAlignment.start,
      children: [
        Row(
          children: [
            SizedBox(
              width: 20,
            ),
            Text(
              title,
              style: TextStyle(fontSize: 12),
            ),
            SizedBox(
              width: 10,
            ),
            Expanded(child: Divider()),
            SizedBox(
              width: 15,
            ),
          ],
        ),
        SizedBox(
          height: 10,
        ),
        ...children,
        SizedBox(
          height: 10,
        )
      ],
    );
  }
}

class DrawerListTile extends StatelessWidget {
  const DrawerListTile({
    super.key,
    required this.title,
    required this.svgSrc,
    required this.press,
    this.active = false,
  });

  final String title, svgSrc;
  final VoidCallback press;
  final bool active;

  @override
  Widget build(BuildContext context) {
    return Material(
      color: active ? Color(0xff0059FF) : Colors.transparent,
      child: InkWell(
        onTap: () {
          if (context.mobile) {
            context.pop();
          }
          press();
        },
        child: Padding(
          padding:
              const EdgeInsets.only(left: 20, right: 15, bottom: 10, top: 10),
          child: Row(
            children: [
              SvgPicture.asset(
                svgSrc,
                color: Colors.white,
                height: 18,
              ),
              SizedBox(
                width: 20,
              ),
              Text(
                title,
                style: TextStyle(
                    color: Colors.white,
                    fontWeight: active ? FontWeight.bold : FontWeight.normal),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
