import 'package:aurcache/components/dashboard/search_field.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import '../../constants/color_constants.dart';
import '../../utils/responsive.dart';

class Header extends StatelessWidget {
  const Header({
    Key? key,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return Row(
      children: [
        if (!context.desktop)
          IconButton(
            icon: const Icon(Icons.menu),
            onPressed: () {
              Scaffold.of(context).openDrawer();
            },
          ),
        if (!context.mobile)
          Column(
            mainAxisAlignment: MainAxisAlignment.start,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                "Hi, Arch User :)",
                style: Theme.of(context).textTheme.titleLarge,
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
        if (!context.mobile) Spacer(flex: context.desktop ? 2 : 1),
        Expanded(child: SearchField()),
        ElevatedButton.icon(
          style: TextButton.styleFrom(
            backgroundColor: darkgreenColor,
            padding: EdgeInsets.symmetric(
              horizontal: defaultPadding * 1.5,
              vertical: defaultPadding / (context.mobile ? 2 : 1),
            ),
          ),
          onPressed: () {
            context.push("/aur");
          },
          icon: const Icon(Icons.add),
          label: const Text(
            "Add New",
          ),
        ),
        //ProfileCard()
      ],
    );
  }
}
