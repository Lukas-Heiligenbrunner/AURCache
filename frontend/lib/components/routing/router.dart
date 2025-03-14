import 'package:aurcache/screens/Package_settings_screen.dart';
import 'package:aurcache/screens/aur_screen.dart';
import 'package:aurcache/screens/build_screen.dart';
import 'package:aurcache/screens/builds_screen.dart';
import 'package:aurcache/screens/dashboard_screen.dart';
import 'package:aurcache/components/routing/menu_shell.dart';
import 'package:aurcache/screens/package_screen.dart';
import 'package:aurcache/screens/packages_screen.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';

final GlobalKey<NavigatorState> _rootNavigatorKey = GlobalKey<NavigatorState>();
final GlobalKey<NavigatorState> _shellNavigatorKey =
    GlobalKey<NavigatorState>();

final appRouter = GoRouter(
  navigatorKey: _rootNavigatorKey,
  initialLocation: '/',
  routes: [
    ShellRoute(
      navigatorKey: _shellNavigatorKey,
      builder: (context, state, child) {
        return MenuShell(child: child);
      },
      routes: [
        GoRoute(
          path: '/',
          builder: (context, state) => DashboardScreen(),
        ),
        GoRoute(
          path: '/build/:id',
          builder: (context, state) {
            final id = int.parse(state.pathParameters['id']!);
            return BuildScreen(buildID: id);
          },
        ),
        GoRoute(
          path: '/builds',
          builder: (context, state) => const BuildsScreen(),
        ),
        GoRoute(
          path: '/packages',
          builder: (context, state) => PackagesScreen(),
        ),
        GoRoute(
          path: '/aur',
          builder: (context, state) {
            return AurScreen(initalQuery: state.uri.queryParameters["query"]);
          },
        ),
        GoRoute(
          path: '/package/:id',
          builder: (context, state) {
            final id = int.parse(state.pathParameters['id']!);
            return PackageScreen(pkgID: id);
          },
        ),
        GoRoute(
          path: '/package/:id/settings',
          builder: (context, state) {
            final id = int.parse(state.pathParameters['id']!);
            return Packagesettingsscreen(pkgID: id);
          },
        ),
      ],
    ),
  ],
);
