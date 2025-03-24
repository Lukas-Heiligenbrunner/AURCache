import 'package:flutter/widgets.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';
part 'build_log.g.dart';

@riverpod
class BuildLog extends _$BuildLog {
  final scrollController = ScrollController();

  @override
  bool build() {
    return true;
  }

  set follow_log(bool value) {
    state = value;
  }

  void go_to_bottom() {
    _scrollToBottom();
  }

  void go_to_top() {
    final scrollPosition = scrollController.position;
    scrollController.animateTo(
      scrollPosition.minScrollExtent,
      duration: const Duration(milliseconds: 200),
      curve: Curves.easeOut,
    );
  }

  void _scrollToBottom() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      // scroll to bottom
      final scrollPosition = scrollController.position;
      if (scrollPosition.viewportDimension < scrollPosition.maxScrollExtent) {
        scrollController.animateTo(
          scrollPosition.maxScrollExtent,
          duration: const Duration(milliseconds: 200),
          curve: Curves.easeOut,
        );
      }
    });
  }
}
