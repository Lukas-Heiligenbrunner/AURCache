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

  List<Widget> _buildElements(Stats? stats, BuildContext context) {
    final double iconSize = context.desktop ? 64 : 42;
    final buildSuccessRate = stats != null
        ? ((stats.successful_builds + stats.failed_builds) != 0
              ? (stats.successful_builds /
                    (stats.successful_builds + stats.failed_builds))
              : 0)
        : 0;

    final buildSuccess = stats != null
        ? "${(buildSuccessRate * 100).toInt()}%"
        : null;

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
        value: stats?.total_packages.toString() ?? "42",
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
        value: stats?.total_builds.toString() ?? "42",
        trendPercent: ((stats?.total_build_trend ?? 0.42) * 100),
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
        value: stats?.repo_size.readableFileSize() ?? "42.42 MiB",
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
        value: stats?.avg_build_time.readableDuration() ?? "42 Seconds",
        trendPercent: stats?.avg_build_time_trend,
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
        value: buildSuccess ?? "10%",
      ),
    ];
  }

  @override
  Widget build(BuildContext context) {
    return APIBuilder(
      interval: Duration(minutes: 1),
      onData: (Stats stats) {
        final items = _buildElements(stats, context);
        return _buildBanner(items, false);
      },
      onLoad: () {
        final items = _buildElements(null, context);
        return _buildBanner(items, true);
      },
      provider: listStatsProvider,
    );
  }

  Widget _buildBanner(List<Widget> items, bool loading) {
    return ResponsiveBuilder(
      mobile: () {
        return Column(
          children: items
              .map((e) => Skeletonizer(enabled: loading, child: e))
              .toList(growable: false),
        );
      },
      desktop: () {
        return Row(
          children: items
              .map(
                (e) => Expanded(
                  child: Skeletonizer(enabled: loading, child: e),
                ),
              )
              .toList(growable: false),
        );
      },
    );
  }
}
