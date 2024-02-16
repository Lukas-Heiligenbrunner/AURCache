import 'package:aurcache/api/statistics.dart';

import '../../api/API.dart';
import 'BaseProvider.dart';

class StatsProvider extends BaseProvider {
  @override
  loadFuture(context, {dto}) {
    data = API.listStats();
    this.dto = dto;
  }
}
