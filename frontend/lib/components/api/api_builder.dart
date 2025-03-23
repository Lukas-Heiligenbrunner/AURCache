import 'package:flutter/widgets.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:toastification/toastification.dart';
import 'package:visibility_detector/visibility_detector.dart';

class APIBuilder<T> extends ConsumerWidget {
  APIBuilder(
      {super.key,
      this.interval,
      required this.onLoad,
      required this.onData,
      required this.provider,
      this.refreshOnComeback = false});

  final Duration? interval;
  final bool refreshOnComeback;

  final Widget Function() onLoad;
  final Widget Function(T data) onData;
  final AutoDisposeFutureProvider<T> provider;

  bool _hasBeenVisible = false; // Flag to track initial visibility

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final asyncValue = ref.watch(provider);
    final t = asyncValue.when(
      data: (data) => onData(data),
      loading: () => onLoad(),
      error: (error, stack) {
        // Optionally show an error toast or widget.
        WidgetsBinding.instance.addPostFrameCallback((_) => toastification.show(
              title: Text('API Request failed! ${error}'),
              autoCloseDuration: const Duration(seconds: 5),
              type: ToastificationType.error,
            ));
        return Center(child: Text('API Request failed! $error'));
      },
    );

    if (refreshOnComeback) {
      return VisibilityDetector(
          key: key ?? Key(hashCode.toString()),
          onVisibilityChanged: (VisibilityInfo info) {
            if (info.visibleFraction > 0) {
              if (_hasBeenVisible) {
                // This isn't the initial load, so trigger refresh.
                print("widget api data refreshed on comeback!");
                ref.invalidate(provider);
              } else {
                // First time visibility; mark as visible without refreshing.
                _hasBeenVisible = true;
              }
            }
          },
          child: t);
    } else {
      return t;
    }
  }
}
