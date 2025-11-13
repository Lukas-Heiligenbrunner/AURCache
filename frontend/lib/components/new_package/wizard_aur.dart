import 'dart:async';
import 'package:flutter/material.dart';
import '../../models/aur_package.dart';
import '../../providers/aur.dart';
import '../api/api_builder.dart';

class AurWizard extends StatefulWidget {
  AurWizard({super.key, required this.onSelect});
  final void Function(String) onSelect;

  @override
  State<AurWizard> createState() => _AurWizardState();
}

class _AurWizardState extends State<AurWizard> {
  TextEditingController controller = TextEditingController();

  String query = "";

  Timer? timer;
  int? selectedIndex;

  @override
  Widget build(BuildContext context) {
    return Expanded(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
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
            decoration: const InputDecoration(hintText: "Type to search..."),
          ),
          APIBuilder(
            key: ValueKey(query),
            onLoad: () => Center(
              child: Column(
                children: [
                  const SizedBox(height: 15),
                  query.length < 3
                      ? const Text("Type to search for an AUR package")
                      : const Text("loading"),
                ],
              ),
            ),
            onData: (List<AurPackage> data) =>
                (query.length < 3 && data.isEmpty)
                ? Column(
                    children: [
                      const SizedBox(height: 15),
                      Text("Type to search for an AUR package"),
                    ],
                  )
                : Expanded(
                    child: Column(
                      children: [
                        SizedBox(height: 10),
                        Expanded(
                          child: ListView.builder(
                            shrinkWrap: true,
                            itemCount: data.length,
                            itemBuilder: (context, index) {
                              final isSelected = selectedIndex == index;
                              return ListTile(
                                title: Row(
                                  children: [
                                    Text(data[index].name),
                                    SizedBox(width: 3),
                                    Text(
                                      data[index].version,
                                      style: TextStyle(fontSize: 10),
                                    ),
                                  ],
                                ),
                                tileColor: isSelected
                                    ? Colors.blue.withOpacity(0.2)
                                    : Colors.transparent,
                                trailing: isSelected
                                    ? const Icon(
                                        Icons.check_circle,
                                        color: Colors.blue,
                                      )
                                    : null,
                                onTap: () {
                                  setState(() => selectedIndex = index);

                                  widget.onSelect(data[index].name);
                                },
                              );
                            },
                          ),
                        ),
                        SizedBox(height: 15),
                      ],
                    ),
                  ),
            provider: getAurPackagesProvider(query),
          ),
        ],
      ),
    );
  }
}
