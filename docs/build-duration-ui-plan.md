# Build Duration UI

Display the time each build took in the builds table (used on both the
Package page and the All Builds page).

## Backend

No changes needed. The `builds` table already has `start_time` and `end_time`
(Unix timestamps, stored as `Option<i64>`). Both are serialized in the API
response and already parsed by the frontend `Build` model.

## Frontend

### 1. `frontend/lib/models/build.dart`

Add a `duration` getter that computes the difference:

```dart
Duration? get duration {
  if (end_time == null) return null;
  return end_time!.difference(start_time);
}
```

### 2. `frontend/lib/utils/time_formatter.dart`

Add a concise extension on `Duration` (avoids null checks at each call site):

```dart
extension BuildDurationFormatter on Duration {
  String readableBuildDuration() {
    if (inDays > 0) {
      return '${inDays}d ${inHours % 24}h';
    } else if (inHours > 0) {
      return '${inHours}h ${inMinutes % 60}m';
    } else if (inMinutes > 0) {
      final secs = inSeconds % 60;
      return secs > 0 ? '${inMinutes}m ${secs}s' : '${inMinutes}m';
    } else {
      return '${inSeconds}s';
    }
  }
}
```

Output samples: `"5s"`, `"1m 23s"`, `"2h 15m"`, `"3d 4h"`.

### 3. `frontend/lib/components/builds_table.dart`

Insert a Duration column (desktop only, after Date, before Package Name):

- Add `import '../utils/time_formatter.dart';`
- DataColumn: `DataColumn(label: Skeleton.keep(child: Text("Duration")))`
- DataCell: `DataCell(Text(build.duration.readableBuildDuration()))` where
  `duration` falls back to `DateTime.now().difference(start_time)` when
  `end_time` is null (in-progress builds show elapsed time so far).
