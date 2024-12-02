import 'dart:async';

import 'package:aurcache/api/aur.dart';
import 'package:aurcache/components/aur_search_table.dart';
import 'package:aurcache/models/aur_package.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/API.dart';
import '../components/api/ApiBuilder.dart';
import '../constants/color_constants.dart';

class AurScreen extends StatefulWidget {
  const AurScreen({super.key, this.initalQuery});

  final String? initalQuery;

  @override
  State<AurScreen> createState() => _AurScreenState();
}

class _AurScreenState extends State<AurScreen> {
  TextEditingController controller = TextEditingController();
  String query = "";
  Timer? timer;

  @override
  void initState() {
    super.initState();
    if (widget.initalQuery != null) {
      query = widget.initalQuery!;
      controller.text = widget.initalQuery!;
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text("AUR"),
      ),
      body: Padding(
        padding: const EdgeInsets.all(defaultPadding),
        child: Container(
          padding: const EdgeInsets.all(defaultPadding),
          decoration: const BoxDecoration(
            color: secondaryColor,
            borderRadius: BorderRadius.all(Radius.circular(10)),
          ),
          child: SingleChildScrollView(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  "AUR Packages",
                  style: Theme.of(context).textTheme.titleMedium,
                ),
                const Text("Search:"),
                TextField(
                    controller: controller,
                    onChanged: (value) {
                      // cancel old timer if active
                      timer?.cancel();
                      // schedule new timer
                      timer = Timer(const Duration(milliseconds: 300), () {
                        setState(() {
                          query = value;
                        });
                      });
                    },
                    decoration:
                        const InputDecoration(hintText: "Type to search...")),
                SizedBox(
                  width: double.infinity,
                  child: APIBuilder(
                      onLoad: () => Center(
                            child: Column(
                              children: [
                                const SizedBox(
                                  height: 15,
                                ),
                                query.length < 3
                                    ? const Text(
                                        "Type to search for an AUR package")
                                    : const Text("loading")
                              ],
                            ),
                          ),
                      onData: (data) => AurSearchTable(data: data),
                      api: () => API.getAurPackages(query)),
                )
              ],
            ),
          ),
        ),
      ),
    );
  }
}
