import 'package:aurcache/components/dashboard/quick_info_tile.dart';
import 'package:aurcache/utils/file_formatter.dart';
import 'package:aurcache/utils/time_formatter.dart';
import 'package:flutter/material.dart';

import '../../constants/color_constants.dart';
import '../../models/quick_info_data.dart';
import '../../utils/responsive.dart';
import '../../models/stats.dart';

class QuickInfoBanner extends StatelessWidget {
  const QuickInfoBanner({
    super.key,
    required this.stats,
  });

  final Stats stats;

  @override
  Widget build(BuildContext context) {
    final Size _size = MediaQuery.of(context).size;
    return Column(
      children: [
        const SizedBox(height: defaultPadding),
        Responsive(
            mobileChild: _buildBanner(
              crossAxisCount: _size.width < 650 ? 2 : 4,
              childAspectRatio: _size.width < 650 ? 1.2 : 1,
            ),
            desktopChild: _buildBanner(
              childAspectRatio: _size.width < 1400 ? 2.75 : 2.75,
            ))
      ],
    );
  }

  List<QuickInfoData> buildQuickInfoData() {
    return [
      QuickInfoData(
          color: primaryColor,
          icon: Icons.widgets,
          title: "Total Packages",
          subtitle: stats.total_packages.toString()),
      QuickInfoData(
          color: const Color(0xFFFFA113),
          icon: Icons.hourglass_top,
          title: "Enqueued Builds",
          subtitle: stats.enqueued_builds.toString()),
      QuickInfoData(
          color: const Color(0xFFA4CDFF),
          icon: Icons.build,
          title: "Total Builds",
          subtitle: stats.total_builds.toString()),
      QuickInfoData(
          color: const Color(0xFFd50000),
          icon: Icons.storage,
          title: "Repo Size",
          subtitle: stats.repo_storage_size.readableFileSize()),
      QuickInfoData(
          color: const Color(0xFF00F260),
          icon: Icons.timelapse,
          title: "Average Build Time",
          subtitle: stats.avg_build_time.readableDuration()),
    ];
  }

  Widget _buildBanner({int crossAxisCount = 5, double childAspectRatio = 1}) {
    final quickInfo = buildQuickInfoData();
    return GridView.builder(
      physics: const NeverScrollableScrollPhysics(),
      shrinkWrap: true,
      itemCount: quickInfo.length,
      gridDelegate: SliverGridDelegateWithFixedCrossAxisCount(
        crossAxisCount: crossAxisCount,
        crossAxisSpacing: defaultPadding,
        mainAxisSpacing: defaultPadding,
        childAspectRatio: childAspectRatio,
      ),
      itemBuilder: (context, idx) => QuickInfoTile(data: quickInfo[idx]),
    );
  }
}
