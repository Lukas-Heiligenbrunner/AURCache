import 'package:aurcache/components/dashboard/quick_info_tile.dart';
import 'package:aurcache/utils/file_formatter.dart';
import 'package:aurcache/utils/time_formatter.dart';
import 'package:flutter/material.dart';
import 'package:flutter_svg/flutter_svg.dart';

import '../../constants/color_constants.dart';
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
    final double iconSize = context.desktop ? 64 : 42;

    final items = [
      QuickInfoTile(
        icon: SvgPicture.asset("assets/icons/tile/Frame.svg",
            colorFilter: const ColorFilter.mode(Colors.white, BlendMode.srcIn),
            width: iconSize),
        title: "Total Packages",
        value: stats.total_packages.toString(),
        positive: false,
        trend: '10%',
      ),
      QuickInfoTile(
        icon: SvgPicture.asset("assets/icons/tile/graph.svg",
            colorFilter: const ColorFilter.mode(Colors.white, BlendMode.srcIn),
            width: iconSize),
        title: "Total Builds",
        value: stats.total_builds.toString(),
        positive: true,
        trend: '10%',
      ),
      QuickInfoTile(
        icon: SvgPicture.asset("assets/icons/tile/clock.svg",
            colorFilter: const ColorFilter.mode(Colors.white, BlendMode.srcIn),
            width: iconSize),
        title: "Repo Size",
        value: stats.repo_storage_size.readableFileSize(),
        positive: true,
        trend: '10%',
      ),
      QuickInfoTile(
        icon: SvgPicture.asset("assets/icons/tile/folder.svg",
            colorFilter: const ColorFilter.mode(Colors.white, BlendMode.srcIn),
            width: iconSize),
        title: "Average Build Time",
        value: stats.avg_build_time.readableDuration(),
        positive: true,
        trend: '10%',
      ),
      QuickInfoTile(
        icon: AspectRatio(
            aspectRatio: 1,
            child: RotatedBox(
                quarterTurns: 1,
                child: CircularProgressIndicator(
                  value: 0.85,
                  strokeWidth: 8,
                  color: Colors.green,
                  backgroundColor: Color(0xff292e35),
                ))),
        title: "Build Success",
        value:
            "${(stats.total_builds != 0 ? (stats.successful_builds * 100 / stats.total_builds) : 0).toInt()}%",
        positive: false,
        trend: '10%',
      ),
    ];

    return ResponsiveBuilder(
      mobile: () {
        return Column(
          children: [
            items[0],
            SizedBox(
              width: defaultPadding,
              height: defaultPadding,
            ),
            items[1],
            SizedBox(
              width: defaultPadding,
              height: defaultPadding,
            ),
            items[2],
            SizedBox(
              width: defaultPadding,
              height: defaultPadding,
            ),
            items[3],
            SizedBox(
              width: defaultPadding,
              height: defaultPadding,
            ),
            items[4],
          ],
        );
      },
      desktop: () {
        return Row(
          children: [
            Expanded(child: items[0]),
            SizedBox(
              width: defaultPadding,
              height: defaultPadding,
            ),
            Expanded(child: items[1]),
            SizedBox(
              width: defaultPadding,
              height: defaultPadding,
            ),
            Expanded(child: items[2]),
            SizedBox(
              width: defaultPadding,
              height: defaultPadding,
            ),
            Expanded(child: items[3]),
            SizedBox(
              width: defaultPadding,
              height: defaultPadding,
            ),
            Expanded(child: items[4]),
          ],
        );
      },
    );
  }
}
