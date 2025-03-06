import 'package:flutter/widgets.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:toastification/toastification.dart';
import 'package:visibility_detector/visibility_detector.dart';

/// A StateNotifier that handles fetching API data and refreshing it.
class APIDataNotifier<T> extends StateNotifier<AsyncValue<T>> {
  final Future<T> Function() api;

  APIDataNotifier({required this.api}) : super(const AsyncValue.loading()) {
    _fetchData();
  }

  Future<void> _fetchData() async {
    try {
      final data = await api();
      state = AsyncValue.data(data);
    } catch (error, stack) {
      state = AsyncValue.error(error, stack);
    }
  }

  /// Call this to re-fetch API data.
  Future<void> refresh() async {
    state = const AsyncValue.loading();
    await _fetchData();
  }
}

StateNotifierProviderFamily<APIDataNotifier<T>, AsyncValue<T>,
    Future<T> Function()> createAPIDataNotifierProvider<T>() {
  return StateNotifierProvider.family<APIDataNotifier<T>, AsyncValue<T>,
      Future<T> Function()>(
    (ref, api) => APIDataNotifier<T>(api: api),
  );
}

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
  bool _hasBeenVisible = false; // Flag to track initial visibility

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
              if (_hasBeenVisible) {
                // This isn't the initial load, so trigger refresh.
                print("widget api data refreshed on comeback!");
                _refreshData();
              } else {
                // First time visibility; mark as visible without refreshing.
                _hasBeenVisible = true;
              }
            }
          },
          child: builder);
    } else {
      return builder;
    }
  }
}
