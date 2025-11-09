import 'package:aurcache/components/aur_search_table.dart';
import 'package:aurcache/components/new_package/wizard_architecture.dart';
import 'package:aurcache/components/new_package/wizard_aur.dart';
import 'package:aurcache/components/new_package/wizard_git.dart';
import 'package:aurcache/components/new_package/wizard_zip.dart';
import 'package:aurcache/constants/platforms.dart';
import 'package:aurcache/screens/aur_screen.dart';
import 'package:flutter/material.dart';
import 'package:flutter_svg/svg.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';
import 'package:step_progress/step_progress.dart';

import 'add_package_popup.dart';

class AddPackagePopupNew extends StatefulWidget {
  const AddPackagePopupNew({super.key});

  @override
  State<AddPackagePopupNew> createState() => _AddPackagePopupNewState();
}

class _AddPackagePopupNewState extends State<AddPackagePopupNew> {
  StepProgressController stepController = StepProgressController(totalSteps: 3);
  int currentStep = 0;
  int? selectedSource;

  @override
  void initState() {
    super.initState();
    currentStep = 0;
  }

  @override
  Widget build(BuildContext context) {
    return Stack(
      children: <Widget>[
        GestureDetector(
          onTap: () {
            Navigator.of(context).pop(false); // Dismiss dialog on outside tap
          },
          child: Container(
            color:
                Colors.black.withValues(alpha: 0.5), // Adjust opacity for blur
          ),
        ),
        // Delete confirmation dialog
        AlertDialog(
          title: Text("Add Package"),
          content: SizedBox(
            height: (currentStep == 1 && selectedSource == 0) ? 400 : 270,
            width: (currentStep == 1 && selectedSource == 0) ? 600 : 430,
            child: Column(
              mainAxisAlignment: MainAxisAlignment.spaceBetween,
              children: [
                (switch (currentStep) {
                  0 => Column(
                      children: [
                        Row(
                          mainAxisAlignment: MainAxisAlignment.center,
                          children: [
                            SquareImageButton(
                              asset: "assets/icons/Archlinux.svg",
                              title: "Official AUR",
                              active: selectedSource == 0,
                              onclick: () {
                                setState(() {
                                  selectedSource = 0;
                                });
                                stepController.setCurrentStep(0);
                              },
                              width: 100,
                            ),
                            SquareImageButton(
                              asset: "assets/icons/git.svg",
                              title: "Git Repo",
                              active: selectedSource == 1,
                              onclick: () {
                                setState(() {
                                  selectedSource = 1;
                                });
                                stepController.setCurrentStep(0);
                              },
                              width: 100,
                            ),
                            SquareImageButton(
                              asset: "assets/icons/zip-icon.svg",
                              title: "Zip Upload",
                              active: selectedSource == 2,
                              onclick: () {
                                setState(() {
                                  selectedSource = 2;
                                });
                                stepController.setCurrentStep(0);
                              },
                              width: 100,
                            )
                          ],
                        ),
                        const Text(
                            "Select the source type of your package you want to build"),
                      ],
                    ),
                  1 => switch (selectedSource) {
                      0 => AurWizard(),
                      1 => GitWizard(),
                      2 => ZipWizard(),
                      _ => Container()
                    },
                  2 => ArchitectureWizard(),
                  _ => Text("test"),
                }),
                StepProgress(
                  totalSteps: 3,
                  padding: const EdgeInsets.symmetric(horizontal: 24),
                  controller: stepController,
                  onStepChanged: (currentIndex) {
                    setState(() {
                      currentStep = currentIndex;
                    });
                  },
                ),
              ],
            ),
          ),
          actions: <Widget>[
            TextButton(
              onPressed: () {
                Navigator.of(context).pop(false); // Dismiss dialog
              },
              child: const Text('Cancel'),
            ),
            TextButton(
              onPressed: !(selectedSource == null ||
                      (currentStep == 1 && selectedSource == 2))
                  ? () {
                      if (currentStep == 2) {
                        Navigator.of(context).pop(true);
                        //successCallback(selectedArchs);
                      } else {
                        stepController.nextStep();
                      }
                    }
                  : null,
              child:
                  currentStep == 2 ? const Text('Install') : const Text('Next'),
            ),
          ],
        ),
      ],
    );
  }
}

Future<bool> showPackageAddPopupNew(
  BuildContext context,
) async {
  return (await showDialog<bool>(
    context: context,
    barrierDismissible: false,
    builder: (BuildContext context) => AddPackagePopupNew(),
  ))!;
}

class SquareImageButton extends StatelessWidget {
  const SquareImageButton(
      {super.key,
      required this.asset,
      required this.title,
      required this.active,
      required this.onclick,
      required this.width});
  final String asset, title;
  final bool active;
  final double width;
  final void Function() onclick;

  @override
  Widget build(BuildContext context) {
    return InkWell(
      splashFactory: NoSplash.splashFactory,
      onTap: onclick,
      child: Padding(
        padding:
            const EdgeInsets.only(left: 10, right: 10, top: 15, bottom: 30),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            SizedBox(
              width: width,
              height: width,
              child: Container(
                decoration: BoxDecoration(
                  border: Border.all(
                      width: active ? 3.0 : 0.0,
                      color: Colors.deepPurpleAccent),
                  borderRadius: const BorderRadius.all(Radius.circular(20.0)),
                  shape: BoxShape.rectangle,
                ),
                child: Padding(
                  padding: const EdgeInsets.all(8.0),
                  child: ClipRRect(
                    borderRadius: BorderRadius.circular(20.0),
                    child: SvgPicture.asset(
                      asset,
                    ),
                  ),
                ),
              ),
            ),
            const SizedBox(
              height: 10,
            ),
            Text(
              title,
              style: TextStyle(color: active ? Colors.deepPurpleAccent : null),
            )
          ],
        ),
      ),
    );
  }
}
