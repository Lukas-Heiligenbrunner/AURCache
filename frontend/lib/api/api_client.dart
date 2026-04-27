import 'package:dio/dio.dart';
import 'package:flutter/foundation.dart';

class ApiClient {
  static String get _apiBase => kDebugMode
      ? "http://localhost:8080/api"
      : Uri.base.resolve("api/").toString();
  final Dio _dio = Dio(BaseOptions(baseUrl: _apiBase));

  ApiClient();

  Dio getRawClient() {
    return _dio;
  }
}
