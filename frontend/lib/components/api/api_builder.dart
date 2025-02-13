
import 'package:flutter/widgets.dart';
import 'package:toastification/toastification.dart';
import 'package:visibility_detector/visibility_detector.dart';

class APIController<T> extends ChangeNotifier {
  void Function()? _refresh;

  // Internal method to bind the refresh function from the state.
  void _attachRefresh(void Function() refreshCallback) {
    _refresh = refreshCallback;
  }

  // Public method to trigger a refresh.
  void refresh() {
    _refresh?.call();
    notifyListeners();
  }
}

class APIBuilder<T> extends StatefulWidget {
  const APIBuilder(
      {super.key,
      this.interval,
      required this.onLoad,
      required this.onData,
      required this.api,
      this.controller,
      this.refreshOnComeback = false});

  final Duration? interval;
  final bool refreshOnComeback;

  final Widget Function() onLoad;
  final Widget Function(T data) onData;
  final Future<T> Function() api;
  final APIController<T>? controller;

  @override
  State<APIBuilder<T>> createState() => _APIBuilderState<T>();
}

class _APIBuilderState<T> extends State<APIBuilder<T>> {
  late Future<T> _futureData;

  @override
  void initState() {
    super.initState();
    _futureData = widget.api();

    // Attach the refresh callback to the controller.
    widget.controller?._attachRefresh(_refreshData);
  }

  // Method to refresh data.
  void _refreshData() {
    setState(() {
      _futureData = widget.api();
    });
  }

  @override
  Widget build(BuildContext context) {
    final builder = FutureBuilder<T>(
      future: _futureData,
      builder: (context, snapshot) {
        if (snapshot.hasError) {
          print(snapshot.error);
          WidgetsBinding.instance
              .addPostFrameCallback((_) => toastification.show(
                    title: Text('API Request failed! ${snapshot.error}'),
                    autoCloseDuration: const Duration(seconds: 5),
                    type: ToastificationType.error,
                  ));
        }
        if (snapshot.hasData) {
          return widget.onData(snapshot.data as T);
        } else {
          return widget.onLoad();
        }
      },
    );

    if (widget.refreshOnComeback) {
      return VisibilityDetector(
          key: widget.key ?? Key(hashCode.toString()),
          onVisibilityChanged: (VisibilityInfo info) {
            if (info.visibleFraction > 0) {
              _refreshData();
            }
          },
          child: builder);
    } else {
      return builder;
    }
  }
}
