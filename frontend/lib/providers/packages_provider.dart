import 'package:aurcache/api/packages.dart';
import 'package:aurcache/providers/BaseProvider.dart';

import '../api/API.dart';

class PackagesProvider extends BaseProvider {
  @override
  loadFuture(context, {dto}) {
    data = API.listPackages();
  }
}
