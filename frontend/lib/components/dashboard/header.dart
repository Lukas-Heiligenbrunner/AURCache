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
        if (!Responsive.isDesktop(context))
          IconButton(
            icon: const Icon(Icons.menu),
            onPressed: () {},
          ),
        if (!Responsive.isMobile(context))
          Column(
            mainAxisAlignment: MainAxisAlignment.start,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                "Hello, Arch User 👋",
                style: Theme.of(context).textTheme.headline6,
              ),
              const SizedBox(
                height: 8,
              ),
              Text(
                "Welcome to your Build server",
                style: Theme.of(context).textTheme.subtitle2,
              ),
            ],
          ),
        if (!Responsive.isMobile(context))
          Spacer(flex: Responsive.isDesktop(context) ? 2 : 1),
        Expanded(child: SearchField()),
        ElevatedButton.icon(
          style: TextButton.styleFrom(
            backgroundColor: darkgreenColor,
            padding: EdgeInsets.symmetric(
              horizontal: defaultPadding * 1.5,
              vertical: defaultPadding / (Responsive.isMobile(context) ? 2 : 1),
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
