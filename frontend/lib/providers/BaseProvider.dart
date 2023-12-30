import 'package:flutter/material.dart';

abstract class BaseProvider<T, DTO> with ChangeNotifier {
  late Future<T> data;

  loadFuture(context, {DTO? dto});

  refresh(context, {DTO? dto}) {
    loadFuture(context, dto: dto);
    notifyListeners();
  }
}
