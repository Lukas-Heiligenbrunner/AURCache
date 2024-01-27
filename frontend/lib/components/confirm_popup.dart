import 'package:flutter/material.dart';

Future<bool> showConfirmationDialog(
  BuildContext context,
  String title,
  String content,
  void Function() successCallback,
  void Function()? errorCallback,
) async {
  return (await showDialog<bool>(
    context: context,
    barrierDismissible: false,
    builder: (BuildContext context) {
      return Stack(
        children: <Widget>[
          GestureDetector(
            onTap: () {
              Navigator.of(context).pop(false); // Dismiss dialog on outside tap
            },
            child: Container(
              color: Colors.black.withOpacity(0.5), // Adjust opacity for blur
            ),
          ),
          // Delete confirmation dialog
          AlertDialog(
            title: Text(title),
            content: Text(content),
            actions: <Widget>[
              TextButton(
                onPressed: () {
                  Navigator.of(context).pop(true);
                  successCallback();
                },
                child: const Text('Yes'),
              ),
              TextButton(
                onPressed: () {
                  Navigator.of(context).pop(false); // Dismiss dialog
                  if (errorCallback != null) {
                    errorCallback();
                  }
                },
                child: const Text('Cancel'),
              ),
            ],
          ),
        ],
      );
    },
  ))!;
}
