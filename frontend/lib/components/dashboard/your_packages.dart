import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/components/packages_table.dart';
import 'package:aurcache/providers/api/packages_provider.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import '../../constants/color_constants.dart';
import '../../models/simple_packge.dart';
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
            style: Theme.of(context).textTheme.titleMedium,
          ),
          APIBuilder<PackagesProvider, List<SimplePackage>, PackagesDTO>(
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
