import 'package:dio/dio.dart';
import 'package:flutter/foundation.dart';

class ApiClient {
  static const String _apiBase =
      kDebugMode ? "https://aurcache.heili.eu/api" : "api";
  final Dio _dio = Dio(BaseOptions(baseUrl: _apiBase));

  String? token;
  DateTime? tokenValidUntil;

  ApiClient();

  Dio getRawClient() {
    return _dio;
  }
}
