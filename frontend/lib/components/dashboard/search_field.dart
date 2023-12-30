import 'package:aurcache/api/packages.dart';
import 'package:aurcache/providers/builds_provider.dart';
import 'package:aurcache/providers/stats_provider.dart';
import 'package:flutter/material.dart';
import 'package:flutter_svg/svg.dart';
import 'package:provider/provider.dart';

import '../../api/API.dart';
import '../../constants/color_constants.dart';
import '../../providers/packages_provider.dart';

class SearchField extends StatelessWidget {
  SearchField({
    Key? key,
  }) : super(key: key);

  final controller = TextEditingController();

  @override
  Widget build(BuildContext context) {
    return TextField(
      controller: controller,
      decoration: InputDecoration(
        hintText: "Search",
        fillColor: secondaryColor,
        filled: true,
        border: const OutlineInputBorder(
          borderSide: BorderSide.none,
          borderRadius: BorderRadius.all(Radius.circular(10)),
        ),
        suffixIcon: InkWell(
          onTap: () async {
            // todo this is only temporary -> add this to a proper page
            await API.addPackage(name: controller.text, force: true);
            Provider.of<PackagesProvider>(context, listen: false)
                .refresh(context);
            Provider.of<BuildsProvider>(context, listen: false)
                .refresh(context);
            Provider.of<StatsProvider>(context, listen: false).refresh(context);
          },
          child: Container(
            padding: EdgeInsets.all(defaultPadding * 0.75),
            margin: EdgeInsets.symmetric(horizontal: defaultPadding / 2),
            decoration: const BoxDecoration(
              color: darkgreenColor,
              borderRadius: BorderRadius.all(Radius.circular(10)),
            ),
            child: SvgPicture.asset(
              "assets/icons/Search.svg",
            ),
          ),
        ),
      ),
    );
  }
}
