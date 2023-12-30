import 'package:flutter/material.dart';

Future<bool> showDeleteConfirmationDialog(BuildContext context) async {
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
            title: Text('Confirm Delete'),
            content: Text('Are you sure you want to delete this item?'),
            actions: <Widget>[
              TextButton(
                onPressed: () {
                  Navigator.of(context).pop(true);
                },
                child: Text('Yes, Delete'),
              ),
              TextButton(
                onPressed: () {
                  Navigator.of(context).pop(false); // Dismiss dialog
                },
                child: Text('Cancel'),
              ),
            ],
          ),
        ],
      );
    },
  ))!;
}
