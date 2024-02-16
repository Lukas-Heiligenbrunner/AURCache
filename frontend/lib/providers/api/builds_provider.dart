import 'package:aurcache/api/builds.dart';

import '../../api/API.dart';
import '../../models/build.dart';
import 'BaseProvider.dart';

class BuildsDTO {
  final int? pkgID;
  final int? limit;

  BuildsDTO({this.pkgID, this.limit});
}

class BuildsProvider extends BaseProvider<List<Build>, BuildsDTO> {
  @override
  loadFuture(context, {dto}) {
    if (dto != null) {
      data = API.listAllBuilds(pkgID: dto.pkgID, limit: dto.limit);
      this.dto = dto;
    } else {
      data = API.listAllBuilds();
    }
  }
}
