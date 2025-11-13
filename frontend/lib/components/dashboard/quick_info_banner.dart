import 'package:aurcache/components/dashboard/quick_info_tile.dart';
import 'package:aurcache/providers/statistics.dart';
import 'package:aurcache/utils/file_formatter.dart';
import 'package:aurcache/utils/time_formatter.dart';
import 'package:flutter/material.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:skeletonizer/skeletonizer.dart';

import '../../models/stats.dart';
import '../../utils/responsive.dart';
import '../api/api_builder.dart';

class QuickInfoBanner extends StatelessWidget {
  const QuickInfoBanner({super.key});

  @override
  Widget build(BuildContext context) {
    return APIBuilder(
      interval: Duration(minutes: 1),
      onData: (Stats stats) {
        return _buildBanner(stats, context);
      },
      onLoad: () {
        return _buildBanner(Stats.dummy(), context);
      },
      provider: listStatsProvider,
    );
  }

  List<Widget> _buildElements(Stats stats, BuildContext context) {
    final double iconSize = context.desktop ? 64 : 42;
    final buildSuccessRate =
        ((stats.successful_builds + stats.failed_builds) != 0
        ? (stats.successful_builds /
              (stats.successful_builds + stats.failed_builds))
        : 0);

    final buildSuccess = "${(buildSuccessRate * 100).toInt()}%";

    return [
      QuickInfoTile(
        icon: Skeleton.shade(
          child: SvgPicture.asset(
            "assets/icons/tile/Frame.svg",
            colorFilter: const ColorFilter.mode(Colors.white, BlendMode.srcIn),
            width: iconSize,
          ),
        ),
        title: "Total Packages",
        value: stats.total_packages.toString(),
      ),
      QuickInfoTile(
        icon: Skeleton.shade(
          child: SvgPicture.asset(
            "assets/icons/tile/graph.svg",
            colorFilter: const ColorFilter.mode(Colors.white, BlendMode.srcIn),
            width: iconSize,
          ),
        ),
        title: "Total Builds",
        value: stats.total_builds.toString(),
        trendPercent: ((stats.total_build_trend) * 100),
      ),
      QuickInfoTile(
        icon: Skeleton.shade(
          child: SvgPicture.asset(
            "assets/icons/tile/folder.svg",
            colorFilter: const ColorFilter.mode(Colors.white, BlendMode.srcIn),
            width: iconSize,
          ),
        ),
        title: "Repo Size",
        value: stats.repo_size.readableFileSize(),
      ),
      QuickInfoTile(
        icon: Skeleton.shade(
          child: SvgPicture.asset(
            "assets/icons/tile/clock.svg",
            colorFilter: const ColorFilter.mode(Colors.white, BlendMode.srcIn),
            width: iconSize,
          ),
        ),
        title: "Average Build Time",
        value: stats.avg_build_time.readableDuration(),
        trendPercent: stats.avg_build_time_trend,
      ),
      QuickInfoTile(
        icon: Skeleton.shade(
          child: AspectRatio(
            aspectRatio: 1,
            child: RotatedBox(
              quarterTurns: -1,
              child: CircularProgressIndicator(
                value: buildSuccessRate.toDouble(),
                strokeWidth: 8,
                color: Color(0xffA9FF0F),
                backgroundColor: Color(0xff292e35),
              ),
            ),
          ),
        ),
        title: "Build Success",
        value: buildSuccess,
      ),
    ];
  }

  Widget _buildBanner(Stats stats, BuildContext context) {
    final items = _buildElements(stats, context);

    return ResponsiveBuilder(
      mobile: () {
        return Column(children: items.map((e) => e).toList(growable: false));
      },
      desktop: () {
        return Row(
          children: items
              .map((e) => Expanded(child: e))
              .toList(growable: false),
        );
      },
    );
  }
}
