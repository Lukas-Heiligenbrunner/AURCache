import 'package:aurcache/components/dashboard/header.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';

class ActivityLog extends StatefulWidget {
  const ActivityLog({super.key});

  @override
  State<ActivityLog> createState() => _ActivityLogState();
}

class _ActivityLogState extends State<ActivityLog> {
  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        ActivityLogItem(
          text: "added Package \"Power\"",
          timestamp: DateTime.timestamp(),
          user: "Lukas Heiligenbrunner",
        ),
        ActivityLogItem(
          text: "added Package \"Naps\"",
          timestamp: DateTime.timestamp(),
          user: "Evin Arslan",
        ),
        ActivityLogItem(
          text: "added Package \"Not\"",
          timestamp: DateTime.timestamp(),
          user: "Sophie Francz",
        ),
        ActivityLogItem(
          text: "added Package \"Powerapps\"",
          timestamp: DateTime.timestamp(),
          user: "Lukas Kessler",
        )
      ],
    );
  }
}

class ActivityLogItem extends StatelessWidget {
  const ActivityLogItem(
      {super.key, this.user, required this.text, required this.timestamp});

  final String? user;
  final String text;
  final DateTime timestamp;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.all(8.0),
      child: Row(
        children: [
          Icon(
            Icons.circle_outlined,
            size: 16,
            color: Color(0xff393C42),
          ),
          SizedBox(
            width: 10,
          ),
          Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Text(
                    user ?? "Unknown User",
                    style: TextStyle(fontWeight: FontWeight.bold, fontSize: 19),
                  ),
                  if (context.desktop)
                    SizedBox(
                      width: 5,
                    ),
                  if (context.desktop)
                    Text(
                      text,
                      style: TextStyle(fontSize: 16),
                    )
                ],
              ),
              context.desktop
                  ? Text(
                      timestamp.toString(),
                      style: TextStyle(fontSize: 15),
                    )
                  : Text(
                      text,
                      style: TextStyle(fontSize: 16),
                    )
            ],
          ),
        ],
      ),
    );
  }
}
