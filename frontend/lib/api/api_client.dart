import 'package:dio/dio.dart';
import 'package:flutter/foundation.dart';

class ApiClient {
  static const String _apiBase =
      kDebugMode ? "http://localhost:8080/api" : "api";
  final Dio _dio = Dio(BaseOptions(baseUrl: _apiBase));

  ApiClient();

  Dio getRawClient() {
    return _dio;
  }
}
