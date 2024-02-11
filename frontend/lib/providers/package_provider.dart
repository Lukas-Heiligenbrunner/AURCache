import 'package:aurcache/api/packages.dart';

import '../api/API.dart';
import '../models/package.dart';
import 'BaseProvider.dart';

class PackageDTO {
  final int pkgID;

  PackageDTO({required this.pkgID});
}

class PackageProvider extends BaseProvider<Package, PackageDTO> {
  @override
  loadFuture(context, {dto}) {
    // todo search solution to force an exising dto
    data = API.getPackage(dto!.pkgID);
    this.dto = dto;
  }
}
