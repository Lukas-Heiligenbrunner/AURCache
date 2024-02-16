import 'package:aurcache/api/builds.dart';

import '../../api/API.dart';
import '../../models/build.dart';
import 'BaseProvider.dart';

class BuildDTO {
  final int buildID;

  BuildDTO({required this.buildID});
}

class BuildProvider extends BaseProvider<Build, BuildDTO> {
  @override
  loadFuture(context, {dto}) {
    // todo search solution to force an exising dto
    data = API.getBuild(dto!.buildID);
    this.dto = dto;
  }
}
