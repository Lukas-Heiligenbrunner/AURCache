import 'package:dio/dio.dart';
import 'package:flutter/foundation.dart';

class ApiClient {
  static const String _apiBase = kDebugMode ? "http://localhost:8081" : "";
  final Dio _dio = Dio(BaseOptions(baseUrl: _apiBase));

  String? token;
  DateTime? tokenValidUntil;

  ApiClient();

  Dio getRawClient() {
    return _dio;
  }
}
