import 'package:aurcache/components/routing/router.dart';
import 'package:device_preview/device_preview.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:google_fonts/google_fonts.dart';
import 'package:toastification/toastification.dart';
import 'constants/color_constants.dart';

void main() {
  GoRouter.optionURLReflectsImperativeAPIs = true;
  runApp(DevicePreview(
    enabled: !kReleaseMode && !kIsWeb,
    builder: (context) => const MyApp(), // Wrap your app
  ));
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
            appBarTheme:
                const AppBarTheme(backgroundColor: bgColor, elevation: 0),
            scaffoldBackgroundColor: bgColor,
            primaryColor: greenColor,
            dialogBackgroundColor: secondaryColor,
            textTheme:
                GoogleFonts.openSansTextTheme(Theme.of(context).textTheme)
                    .apply(bodyColor: Colors.white),
            canvasColor: secondaryColor,
            drawerTheme: ThemeData.dark()
                .drawerTheme
                .copyWith(backgroundColor: Color(0xff131418))),
      ),
    );
  }
}
