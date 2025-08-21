import 'package:aurcache/components/api/api_builder.dart';
import 'package:aurcache/providers/statistics.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:skeletonizer/skeletonizer.dart';
import '../../constants/color_constants.dart';
import '../../models/user_info.dart';
import '../../utils/responsive.dart';

class Header extends StatelessWidget {
  const Header({super.key});

  @override
  Widget build(BuildContext context) {
    return Row(
      children: [
        if (context.mobile)
          IconButton(
            icon: const Icon(Icons.menu),
            onPressed: () {
              Scaffold.of(context).openDrawer();
            },
          ),
        if (context.desktop)
          Column(
            mainAxisAlignment: MainAxisAlignment.start,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              APIBuilder(
                onLoad: () {
                  return Skeletonizer(
                      child: Text(
                    "Hi, Arch User :)",
                    style: Theme.of(context).textTheme.titleLarge,
                  ));
                },
                onData: (UserInfo data) {
                  return Text(
                    data.username == null
                        ? "Hi, Arch User :)"
                        : "Hi, ${data.username} :)",
                    style: Theme.of(context).textTheme.titleLarge,
                  );
                },
                provider: userInfoProvider,
              ),
              const SizedBox(
                height: 8,
              ),
              Text(
                "Welcome to your personal build server",
                style: Theme.of(context).textTheme.titleSmall,
              ),
            ],
          ),
        Spacer(flex: context.desktop ? 2 : 1),
        OutlinedButton.icon(
          style: OutlinedButton.styleFrom(
            backgroundColor: Color(0xff0059FF),
            side: BorderSide(color: Color(0xff0059FF), width: 0),
            shape:
                RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
            padding: EdgeInsets.symmetric(
              horizontal: defaultPadding,
              vertical: defaultPadding / (context.mobile ? 2 : 1),
            ),
          ),
          onPressed: () {
            context.push("/aur");
          },
          icon: const Icon(
            Icons.add,
            color: Colors.white,
          ),
          label: const Text(
            "Add Package",
            style: TextStyle(color: Colors.white),
          ),
        ),
        //ProfileCard()
      ],
    );
  }
}
