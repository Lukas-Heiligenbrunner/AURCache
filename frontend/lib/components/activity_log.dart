import 'package:aurcache/api/activity_log.dart';
import 'package:aurcache/components/api/api_builder.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:skeletonizer/skeletonizer.dart';

import '../api/API.dart';

class ActivityLog extends StatefulWidget {
  const ActivityLog({super.key});

  @override
  State<ActivityLog> createState() => _ActivityLogState();
}

class _ActivityLogState extends State<ActivityLog> {
  @override
  Widget build(BuildContext context) {
    return APIBuilder(
        onLoad: () => Skeletonizer(
              enabled: true,
              child: Column(
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
              ),
            ),
        onData: (v) => Column(
            children: v
                .map(
                  (e) => ActivityLogItem(
                    text: e.text,
                    timestamp: e.timestamp,
                    user: e.user,
                  ),
                )
                .toList(growable: false)),
        api: API.listActivities);
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
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Icon(
              Icons.circle_outlined,
              size: 16,
              color: Color(0xff393C42),
            ),
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
                    user ?? "You",
                    style: TextStyle(fontWeight: FontWeight.bold, fontSize: 18),
                  ),
                  if (context.desktop)
                    SizedBox(
                      width: 5,
                    ),
                  if (context.desktop)
                    Text(
                      text,
                      style: TextStyle(fontSize: 18),
                    )
                ],
              ),
              context.desktop
                  ? Text(
                      timestamp.toString(),
                      style: TextStyle(fontSize: 16, color: Colors.white70),
                    )
                  : Text(
                      text,
                      style: TextStyle(fontSize: 14),
                    )
            ],
          ),
        ],
      ),
    );
  }
}
