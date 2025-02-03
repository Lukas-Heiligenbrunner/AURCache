import 'package:dio/dio.dart';
import 'package:flutter/foundation.dart';
import 'package:url_launcher/url_launcher.dart';

class ApiClient {
  static const String _apiBase =
      kDebugMode ? "http://localhost:8080/api" : "api";
  final Dio _dio = Dio(BaseOptions(baseUrl: _apiBase))..interceptors.add(InterceptorsWrapper(
    onRequest: (RequestOptions options, RequestInterceptorHandler handler) {
      // Do something before request is sent.
      // If you want to resolve the request with custom data,
      // you can resolve a `Response` using `handler.resolve(response)`.
      // If you want to reject the request with a error message,
      // you can reject with a `DioException` using `handler.reject(dioError)`.
      return handler.next(options);
    },
    onResponse: (Response response, ResponseInterceptorHandler handler) {
      // Do something with response data.
      // If you want to reject the request with a error message,
      // you can reject a `DioException` object using `handler.reject(dioError)`.
      print(response.redirects);
      print(response.statusCode);
      return handler.next(response);
    },
    onError: (DioException error, ErrorInterceptorHandler handler) async {
      // Do something with response error.
      // If you want to resolve the request with some custom data,
      // you can resolve a `Response` object using `handler.resolve(response)`.
      if(!kIsWeb && error.response?.statusCode == 401) {
        if (!await launchUrl(Uri.parse(_apiBase + "/login"))) {
          throw Exception('Could not launch url');
        }
      }
      return handler.next(error);
    },
  ));

  ApiClient();

  Dio getRawClient() {
    return _dio;
  }
}
