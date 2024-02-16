import 'package:flutter/material.dart';

abstract class BaseProvider<T, DTO> with ChangeNotifier {
  late Future<T> data;
  DTO? dto;

  loadFuture(context, {DTO? dto});

  refresh(context) {
    loadFuture(context, dto: this.dto);
    notifyListeners();
  }
}
