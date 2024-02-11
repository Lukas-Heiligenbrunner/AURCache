import 'package:aurcache/api/packages.dart';
import 'package:aurcache/providers/BaseProvider.dart';

import '../api/API.dart';
import '../models/package.dart';

class PackagesDTO {
  final int limit;

  PackagesDTO({required this.limit});
}

class PackagesProvider extends BaseProvider<List<Package>, PackagesDTO> {
  @override
  loadFuture(context, {dto}) {
    data = API.listPackages(limit: dto?.limit);
    this.dto = dto;
  }
}
