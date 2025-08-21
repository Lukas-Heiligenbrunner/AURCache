import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';
import 'package:toastification/toastification.dart';

class APIBuilder<T> extends ConsumerStatefulWidget {
  const APIBuilder(
      {super.key,
      this.interval,
      required this.onLoad,
      required this.onData,
      required this.provider});

  final Duration? interval;

  final Widget Function() onLoad;
  final Widget Function(T data) onData;
  final ProviderListenable<AsyncValue<T>> provider;

  @override
  ConsumerState<APIBuilder<T>> createState() => _APIBuilderState<T>();
}

class _APIBuilderState<T> extends ConsumerState<APIBuilder<T>> {
  Timer? timer;

  @override
  Widget build(BuildContext context) {
    final asyncValue = ref.watch(widget.provider);
    return asyncValue.when(
      data: (data) => widget.onData(data),
      loading: () => widget.onLoad(),
      error: (error, stack) {
        // Optionally show an error toast or widget.
        WidgetsBinding.instance.addPostFrameCallback((_) => toastification.show(
              title: Text('API Request failed! $error'),
              autoCloseDuration: const Duration(seconds: 5),
              type: ToastificationType.error,
            ));
        return Center(child: Text('API Request failed! $error'));
      },
    );
  }

  @override
  void initState() {
    super.initState();

    if (widget.interval != null) {
      timer = Timer.periodic(
          widget.interval!, (_) => ref.invalidate(widget.provider as ProviderOrFamily));
    }
  }

  @override
  void dispose() {
    super.dispose();
    timer?.cancel();
  }
}
