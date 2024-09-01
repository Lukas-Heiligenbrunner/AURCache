import 'package:fl_chart/fl_chart.dart';
import 'package:flutter/material.dart';

class BuildsChart extends StatefulWidget {
  const BuildsChart({
    super.key,
    required this.nrBuilds,
    required this.nrSuccessfulBuilds,
    required this.nrfailedbuilds,
    required this.nrEnqueuedBuilds,
  });

  final int nrBuilds;
  final int nrSuccessfulBuilds;
  final int nrfailedbuilds;
  final int nrEnqueuedBuilds;

  @override
  _BuildsChartState createState() => _BuildsChartState();
}

class _BuildsChartState extends State<BuildsChart> {
  int touchedIndex = -1;
  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: 300,
      child: AspectRatio(
        aspectRatio: 1.3,
        child: Row(
          children: <Widget>[
            const SizedBox(
              height: 18,
            ),
            Expanded(
              child: AspectRatio(
                aspectRatio: 1,
                child: PieChart(
                  PieChartData(
                      pieTouchData: PieTouchData(
                          touchCallback: (pieTouchResponse, touchresponse) {
                        setState(() {
                          // todo hover gesture not working properly
                          if (touchresponse?.touchedSection != null) {
                            touchedIndex = touchresponse!
                                .touchedSection!.touchedSectionIndex;
                          } else {
                            touchedIndex = -1;
                          }
                        });
                      }),
                      borderData: FlBorderData(
                        show: false,
                      ),
                      sectionsSpace: 0,
                      centerSpaceRadius: 40,
                      sections: showingSections()),
                ),
              ),
            ),
            const SizedBox(
              width: 28,
            ),
          ],
        ),
      ),
    );
  }

  List<PieChartSectionData> showingSections() {
    return List.generate(3, (i) {
      final isTouched = i == touchedIndex;
      final fontSize = isTouched ? 25.0 : 16.0;
      final radius = isTouched ? 60.0 : 50.0;
      switch (i) {
        case 0:
          return PieChartSectionData(
            color: const Color(0xff760707),
            value: widget.nrfailedbuilds.toDouble(),
            title:
                "${(widget.nrfailedbuilds * 100 / widget.nrBuilds).toStringAsFixed(2)}%",
            radius: radius,
            titleStyle: TextStyle(
                fontSize: fontSize,
                fontWeight: FontWeight.bold,
                color: const Color(0xffffffff)),
          );
        case 1:
          return PieChartSectionData(
            color: const Color(0xff0a7005),
            value: (widget.nrSuccessfulBuilds).toDouble(),
            title:
                "${((widget.nrSuccessfulBuilds) * 100 / widget.nrBuilds).toStringAsFixed(2)}%",
            radius: radius,
            titleStyle: TextStyle(
                fontSize: fontSize,
                fontWeight: FontWeight.bold,
                color: const Color(0xffffffff)),
          );
        case 2:
          return PieChartSectionData(
            color: const Color(0xFF0044AA),
            value: (widget.nrEnqueuedBuilds).toDouble(),
            title:
                "${((widget.nrEnqueuedBuilds) * 100 / widget.nrBuilds).toStringAsFixed(2)}%",
            radius: radius,
            titleStyle: TextStyle(
                fontSize: fontSize,
                fontWeight: FontWeight.bold,
                color: const Color(0xffffffff)),
          );
        default:
          throw Error();
      }
    });
  }
}
