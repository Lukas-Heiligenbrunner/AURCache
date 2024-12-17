import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:go_router/go_router.dart';
import '../../constants/color_constants.dart';
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
        Spacer(flex: context.desktop ? 2 : 1),

        if (context.desktop) ...[
          KeyboardListener(
            focusNode: FocusNode(),
            onKeyEvent: (event) {
              if (event.runtimeType == KeyDownEvent &&
                  event.logicalKey == LogicalKeyboardKey.enter) {
                //context.push("/aur?query=${controller.text}");
              }
            },
            child: SizedBox(
              width: 350,
              child: TextField(
                //controller: controller,
                decoration: InputDecoration(
                  hintText: "Search",
                  fillColor: secondaryColor,
                  filled: true,
                  border: const OutlineInputBorder(
                    borderSide: BorderSide.none,
                    borderRadius: BorderRadius.all(Radius.circular(8)),
                  ),
                ),
              ),
            ),
          ),
          SizedBox(
            width: 15,
          ),
          OutlinedButton.icon(
            style: OutlinedButton.styleFrom(
              backgroundColor: bgColor,
              shape: RoundedRectangleBorder(
                  borderRadius: BorderRadius.circular(8)),
              padding: EdgeInsets.symmetric(
                horizontal: defaultPadding,
                vertical: defaultPadding / (context.mobile ? 2 : 1),
              ),
            ),
            onPressed: () {
              // todo
              // context.push("/aur");
            },
            icon: const Icon(
              Icons.filter_list,
              color: Colors.white54,
            ),
            label: const Text(
              "Filter",
              style: TextStyle(color: Colors.white54),
            ),
          ),
          SizedBox(
            width: 15,
          ),
        ],
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
