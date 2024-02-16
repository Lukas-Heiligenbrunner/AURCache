import 'package:flutter/widgets.dart';

class BuildLogProvider with ChangeNotifier {
  final scrollController = ScrollController();

  bool followLog = true;

  set follow_log(bool value) {
    followLog = value;
    notifyListeners();
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
