import 'package:aurcache/api/aur.dart';
import 'package:aurcache/models/aur_package.dart';

import '../api/API.dart';
import 'BaseProvider.dart';

class AurSearchDTO {
  final String query;

  AurSearchDTO({required this.query});
}

class AURSearchProvider extends BaseProvider<List<AurPackage>, AurSearchDTO> {
  @override
  loadFuture(context, {dto}) {
    data = API.getAurPackages(dto!.query);
  }
}
