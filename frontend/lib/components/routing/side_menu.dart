import 'package:aurcache/constants/versions.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:go_router/go_router.dart';
import 'package:url_launcher/url_launcher.dart';

class SideMenu extends StatelessWidget {
  const SideMenu({super.key});

  @override
  Widget build(BuildContext context) {
    final activeUri = GoRouterState.of(context).uri.toString();

    return Drawer(
      child: CustomScrollView(
        slivers: [
          SliverFillRemaining(
            hasScrollBody: false,
            child: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Column(
                  children: [
                    SizedBox(height: 30),
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
                                  fontWeight: FontWeight.w900,
                                  fontSize: 16,
                                ),
                              ),
                              const Text(
                                "The Archlinux AUR\nbuild server",
                                style: TextStyle(fontSize: 12),
                              ),
                            ],
                          ),
                        ),
                      ],
                    ),
                    SizedBox(height: 20),
                    DrawerSection(
                      title: "General",
                      children: [
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
                          title: "Activities",
                          icon: Icons.list,
                          active: activeUri.startsWith("/activities"),
                          press: () {
                            context.go("/activities");
                          },
                        ),
                      ],
                    ),
                  ],
                ),
                DrawerSection(
                  title: "Settings",
                  children: [
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
                      press: () async {
                        await launchUrl(
                          Uri.parse(
                            "https://lukas-heiligenbrunner.github.io/AURCache/docs/overview/introduction",
                          ),
                          webOnlyWindowName: '_blank',
                        );
                      },
                    ),
                  ],
                ),
                Expanded(child: Container()),
                DrawerSection(
                  title: "Project Infos",
                  children: [
                    DrawerListTile(
                      title: "Github",
                      icon: Icons.open_in_new,
                      press: () async {
                        await launchUrl(
                          Uri.parse(
                            "https://github.com/Lukas-Heiligenbrunner/AURCache",
                          ),
                          webOnlyWindowName: '_blank',
                        );
                      },
                    ),
                    SizedBox(height: 10),
                    Padding(
                      padding: EdgeInsetsGeometry.only(left: 25),
                      child: Row(
                        mainAxisAlignment: MainAxisAlignment.start,
                        children: [
                          Text(
                            "Version $appVersion",
                            style: TextStyle(color: Color(0xff868686)),
                          ),
                        ],
                      ),
                    ),
                    SizedBox(height: 20),
                  ],
                ),
              ],
            ),
          ),
        ],
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
            SizedBox(width: 20),
            Text(title, style: TextStyle(fontSize: 12)),
            SizedBox(width: 10),
            Expanded(child: Divider()),
            SizedBox(width: 15),
          ],
        ),
        SizedBox(height: 10),
        ...children,
        SizedBox(height: 10),
      ],
    );
  }
}

class DrawerListTile extends StatelessWidget {
  const DrawerListTile({
    super.key,
    required this.title,
    this.svgSrc,
    this.icon,
    this.press,
    this.active = false,
  }) : assert(
         svgSrc != null || icon != null,
         'Either svgSrc or icon must be provided.',
       );

  final String title;
  final String? svgSrc;
  final IconData? icon;
  final VoidCallback? press;
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
          press?.call();
        },
        child: Padding(
          padding: const EdgeInsets.only(
            left: 20,
            right: 15,
            bottom: 10,
            top: 10,
          ),
          child: Row(
            children: [
              if (svgSrc != null)
                SvgPicture.asset(svgSrc!, color: Colors.white, height: 18)
              else if (icon != null)
                Icon(icon, color: Colors.white, size: 18),
              SizedBox(width: 20),
              Text(
                title,
                style: TextStyle(
                  color: Colors.white,
                  fontWeight: active ? FontWeight.bold : FontWeight.normal,
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
