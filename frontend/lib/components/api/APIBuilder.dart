import 'dart:async';

import 'package:flutter/material.dart';
import 'package:provider/provider.dart';
import 'package:visibility_detector/visibility_detector.dart';

import '../../providers/BaseProvider.dart';

class APIBuilder<T extends BaseProvider, K, DTO> extends StatefulWidget {
  const APIBuilder(
      {super.key,
      required this.onLoad,
      required this.onData,
      this.interval,
      this.dto});

  final DTO? dto;
  final Duration? interval;
  final Widget Function() onLoad;
  final Widget Function(K t) onData;

  @override
  State<APIBuilder<T, K, DTO>> createState() => _APIBuilderState<T, K, DTO>();
}

class _APIBuilderState<T extends BaseProvider, K, DTO>
    extends State<APIBuilder<T, K, DTO>> {
  Timer? timer;
  bool visible = true;

  @override
  void initState() {
    super.initState();
    Provider.of<T>(context, listen: false).loadFuture(context, dto: widget.dto);

    if (widget.interval != null) {
      timer = Timer.periodic(widget.interval!, (Timer t) {
        if (visible) {
          Provider.of<T>(context, listen: false)
              .refresh(context, dto: widget.dto);
        }
      });
    }
  }

  @override
  void dispose() {
    super.dispose();
    timer?.cancel();
  }

  @override
  Widget build(BuildContext context) {
    final Future<K> fut = Provider.of<T>(context).data as Future<K>;

    return VisibilityDetector(
      key: widget.key ?? const Key("APIBuilder"),
      onVisibilityChanged: (visibilityInfo) {
        var visiblePercentage = visibilityInfo.visibleFraction * 100;

        if (mounted) {
          setState(() {
            visible = visiblePercentage != 0;
          });
        }

        debugPrint(
            'Widget ${visibilityInfo.key} is ${visiblePercentage}% visible');
      },
      child: FutureBuilder<K>(
        future: fut,
        builder: (context, snapshot) {
          if (snapshot.hasData) {
            return widget.onData(snapshot.data!);
          } else {
            return widget.onLoad();
          }
        },
      ),
    );
  }
}
