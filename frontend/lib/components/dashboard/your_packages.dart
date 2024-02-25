import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/components/packages_table.dart';
import 'package:aurcache/providers/api/packages_provider.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import '../../constants/color_constants.dart';
import '../../models/package.dart';
import '../table_info.dart';

class YourPackages extends StatelessWidget {
  const YourPackages({super.key});

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.all(defaultPadding),
      decoration: const BoxDecoration(
        color: secondaryColor,
        borderRadius: BorderRadius.all(Radius.circular(10)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            "Your Packages",
            style: Theme.of(context).textTheme.subtitle1,
          ),
          APIBuilder<PackagesProvider, List<Package>, PackagesDTO>(
            key: const Key("Packages on dashboard"),
            interval: const Duration(seconds: 10),
            dto: PackagesDTO(limit: 10),
            onData: (data) {
              if (data.isEmpty) {
                return const TableInfo(title: "You have no packages yet");
              } else {
                return Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    SizedBox(
                        width: double.infinity,
                        child: PackagesTable(data: data)),
                    ElevatedButton(
                      onPressed: () {
                        context.push("/packages");
                      },
                      child: Text(
                        "List all Packages",
                        style: TextStyle(color: Colors.white.withOpacity(0.8)),
                      ),
                    )
                  ],
                );
              }
            },
            onLoad: () => const CircularProgressIndicator(),
          ),
        ],
      ),
    );
  }
}

IconData switchSuccessIcon(int status) {
  switch (status) {
    case 0:
      return Icons.watch_later_outlined;
    case 1:
      return Icons.check_circle_outline;
    case 2:
      return Icons.cancel_outlined;
    case 3:
      return Icons.pause_circle_outline;
    default:
      return Icons.question_mark_outlined;
  }
}

Color switchSuccessColor(int status) {
  switch (status) {
    case 0:
      return const Color(0xFF9D8D00);
    case 1:
      return const Color(0xFF0A6900);
    case 2:
      return const Color(0xff760707);
    case 3:
      return const Color(0xFF0044AA);
    default:
      return const Color(0xFF9D8D00);
  }
}
