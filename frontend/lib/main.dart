import 'package:aurcache/components/routing/router.dart';
import 'package:device_preview/device_preview.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import 'package:google_fonts/google_fonts.dart';
import 'package:toastification/toastification.dart';
import 'constants/color_constants.dart';

void main() {
  if (kIsWeb) {
    if (bool.fromEnvironment('dart.tool.dart2wasm')) {
      print("You are using the WASM build of Flutter");
    } else {
      print(
        "you are using the JS build of Flutter. Your Browser doesn't support WASM",
      );
    }
  }

  GoRouter.optionURLReflectsImperativeAPIs = true;
  runApp(
    ProviderScope(
      child: DevicePreview(
        enabled: !kReleaseMode && !kIsWeb,
        builder: (context) => const MyApp(), // Wrap your app
      ),
    ),
  );
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return ToastificationWrapper(
      config: ToastificationConfig(alignment: Alignment.bottomRight),
      child: MaterialApp.router(
        routerConfig: appRouter,
        debugShowCheckedModeBanner: false,
        title: 'AURCache',
        theme: ThemeData.dark().copyWith(
          appBarTheme: const AppBarTheme(
            backgroundColor: bgColor,
            elevation: 0,
          ),
          scaffoldBackgroundColor: bgColor,
          primaryColor: greenColor,
          textTheme: GoogleFonts.openSansTextTheme(
            Theme.of(context).textTheme,
          ).apply(bodyColor: Colors.white),
          canvasColor: secondaryColor,
          drawerTheme: ThemeData.dark().drawerTheme.copyWith(
            backgroundColor: Color(0xff131418),
          ),
          dialogTheme: DialogThemeData(backgroundColor: secondaryColor),
        ),
      ),
    );
  }
}
