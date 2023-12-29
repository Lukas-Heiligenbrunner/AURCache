import 'package:aurcache/screens/dashboard/components/chart_card.dart';
import 'package:flutter/material.dart';

import '../../../core/constants/color_constants.dart';
import 'charts.dart';

class UserDetailsWidget extends StatelessWidget {
  const UserDetailsWidget({
    Key? key,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: EdgeInsets.all(defaultPadding),
      decoration: BoxDecoration(
        color: secondaryColor,
        borderRadius: const BorderRadius.all(Radius.circular(10)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            "Package build success",
            style: TextStyle(
              fontSize: 18,
              fontWeight: FontWeight.w500,
            ),
          ),
          SizedBox(height: defaultPadding),
          Chart(),
          UserDetailsMiniCard(
            color: const Color(0xff0a7005),
            title: "Successful Builds",
            amountOfFiles: "%16.7",
            numberOfIncrease: 1328,
          ),
          UserDetailsMiniCard(
            color: const Color(0xff760707),
            title: "Failed Builds",
            amountOfFiles: "%28.3",
            numberOfIncrease: 1328,
          ),
        ],
      ),
    );
  }
}
