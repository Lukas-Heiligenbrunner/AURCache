import 'package:aurcache/components/routing/router.dart';
import 'package:aurcache/providers/build_provider.dart';
import 'package:aurcache/providers/builds_provider.dart';
import 'package:aurcache/providers/package_provider.dart';
import 'package:aurcache/providers/packages_provider.dart';
import 'package:aurcache/providers/stats_provider.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:google_fonts/google_fonts.dart';
import 'package:provider/provider.dart';
import 'constants/color_constants.dart';

void main() {
  GoRouter.optionURLReflectsImperativeAPIs = true;
  runApp(const MyApp());
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MultiProvider(
      providers: [
        ChangeNotifierProvider<StatsProvider>(create: (_) => StatsProvider()),
        ChangeNotifierProvider<PackagesProvider>(
            create: (_) => PackagesProvider()),
        ChangeNotifierProvider<BuildsProvider>(create: (_) => BuildsProvider()),
        ChangeNotifierProvider<PackageProvider>(
            create: (_) => PackageProvider()),
        ChangeNotifierProvider<BuildProvider>(create: (_) => BuildProvider()),
      ],
      child: MaterialApp.router(
        routerConfig: appRouter,
        debugShowCheckedModeBanner: false,
        title: 'AURCache',
        theme: ThemeData.dark().copyWith(
          appBarTheme:
              const AppBarTheme(backgroundColor: bgColor, elevation: 0),
          scaffoldBackgroundColor: bgColor,
          primaryColor: greenColor,
          dialogBackgroundColor: secondaryColor,
          textTheme: GoogleFonts.openSansTextTheme(Theme.of(context).textTheme)
              .apply(bodyColor: Colors.white),
          canvasColor: secondaryColor,
        ),
      ),
    );
  }
}
