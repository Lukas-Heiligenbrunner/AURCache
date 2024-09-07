import 'package:aurcache/api/packages.dart';
import 'package:aurcache/providers/api/BaseProvider.dart';

import '../../api/API.dart';
import '../../models/simple_packge.dart';

class PackagesDTO {
  final int limit;

  PackagesDTO({required this.limit});
}

class PackagesProvider extends BaseProvider<List<SimplePackage>, PackagesDTO> {
  @override
  loadFuture(context, {dto}) {
    data = API.listPackages(limit: dto?.limit);
    this.dto = dto;
  }
}
