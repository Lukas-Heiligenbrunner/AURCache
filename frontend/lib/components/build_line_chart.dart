import 'package:aurcache/api/API.dart';
import 'package:aurcache/api/statistics.dart';
import 'package:aurcache/utils/num_formatter.dart';
import 'package:fl_chart/fl_chart.dart';
import 'package:flutter/material.dart';

class BuildLineChart extends StatefulWidget {
  const BuildLineChart({super.key});

  @override
  State<BuildLineChart> createState() => _BuildLineChartState();
}

class _BuildLineChartState extends State<BuildLineChart> {
  final months = [
    "Jan",
    "Feb",
    "Mar",
    "Apr",
    "May",
    "Jun",
    "Jul",
    "Aug",
    "Sep",
    "Oct",
    "Nov",
    "Dec"
  ];

  List<FlSpot> data = [
    FlSpot(0, 0),
    FlSpot(1, 0),
    FlSpot(2, 0),
    FlSpot(3, 0),
    FlSpot(4, 0),
    FlSpot(5, 0),
    FlSpot(6, 0),
    FlSpot(7, 0),
    FlSpot(8, 0),
    FlSpot(9, 0),
    FlSpot(10, 0),
    FlSpot(11, 0),
  ];
  int maxValue = 10;

  List<Color> gradientColors = [
    Colors.purple,
    Colors.deepPurpleAccent,
  ];

  @override
  void initState() {
    super.initState();
    getGraphData();
  }

  @override
  Widget build(BuildContext context) {
    return AspectRatio(
      aspectRatio: 2,
      child: LineChart(
        mainData(),
      ),
    );
  }

  Widget bottomTitleWidgets(double value, TitleMeta meta) {
    const style = TextStyle(
      fontWeight: FontWeight.bold,
      fontSize: 12,
    );
    final now = DateTime.now();
    final ivalue = value.toInt();

    if (ivalue % 2 == 0 || value % 1 != 0) {
      return SideTitleWidget(
        meta: meta,
        child: Text(""),
      );
    } else {
      final text = Text(
        months[(now.month + ivalue) % 12],
        style: style,
      );

      return SideTitleWidget(
        meta: meta,
        child: text,
      );
    }
  }

  Widget leftTitleWidgets(double value, TitleMeta meta) {
    const style = TextStyle(
      fontWeight: FontWeight.bold,
      fontSize: 12,
    );
    final ivalue = value.toInt();
    if (ivalue % (maxValue ~/ 4) == 0) {
      return Text(value.toInt().shortForm(),
          style: style, textAlign: TextAlign.left);
    } else {
      return Text("", style: style, textAlign: TextAlign.left);
    }
  }

  getGraphData() async {
    final graphdata = await API.getGraphData();

    final spotData = data;
    final currentMonth = DateTime.now().month;
    final currentYear = DateTime.now().year;

    int newMaxValue = maxValue;

    for (final p in graphdata) {
      if (p.year == currentYear) {
        final idx = 12 - currentMonth + p.month - 1;
        spotData[idx] = spotData[idx].copyWith(y: p.count.toDouble());
      } else {
        final idx = p.month - currentMonth - 1;
        spotData[idx] = spotData[idx].copyWith(y: p.count.toDouble());
      }

      if (newMaxValue < p.count) {
        newMaxValue = p.count;
      }
    }

    setState(() {
      data = spotData;
      maxValue = newMaxValue;
    });
  }

  LineChartData mainData() {
    return LineChartData(
        gridData: FlGridData(
          show: true,
          drawVerticalLine: false,
          horizontalInterval: 1,
          verticalInterval: 1,
          getDrawingHorizontalLine: (value) {
            final ivalue = value.toInt();
            if (ivalue % (maxValue ~/ 4) == 0) {
              return FlLine(
                  color: Colors.grey.withOpacity(0.8),
                  strokeWidth: 1,
                  dashArray: [5, 5]);
            } else {
              return FlLine(strokeWidth: 0);
            }
          },
        ),
        titlesData: FlTitlesData(
          show: true,
          rightTitles: const AxisTitles(
            sideTitles: SideTitles(showTitles: false),
          ),
          topTitles: const AxisTitles(
            sideTitles: SideTitles(showTitles: false),
          ),
          bottomTitles: AxisTitles(
            sideTitles: SideTitles(
              showTitles: true,
              reservedSize: 30,
              interval: 1,
              getTitlesWidget: bottomTitleWidgets,
            ),
          ),
          leftTitles: AxisTitles(
            sideTitles: SideTitles(
              showTitles: true,
              interval: 1,
              getTitlesWidget: leftTitleWidgets,
              reservedSize: 30,
            ),
          ),
        ),
        borderData: FlBorderData(
          show: false,
        ),
        minX: -0.25,
        maxX: 11.25,
        minY: 0,
        maxY: maxValue.toDouble() * 1.1,
        lineBarsData: [
          LineChartBarData(
            spots: data,
            isCurved: true,
            gradient: LinearGradient(
              colors: gradientColors,
            ),
            barWidth: 3,
            isStrokeCapRound: true,
            dotData: FlDotData(
              show: true,
              getDotPainter: (spot, percent, barData, index) {
                Color dotColor = Color.lerp(
                  barData.gradient!.colors.first,
                  barData.gradient!.colors.last,
                  percent / 100,
                )!;

                return FlDotCirclePainter(
                  radius: 3.5,
                  color: Colors.white,
                  strokeWidth: 3,
                  strokeColor: dotColor,
                );
              },
              checkToShowDot: (spot, barData) {
                // limitate which dot to show
                return true;
              },
            ),
            belowBarData: BarAreaData(
              show: true,
              gradient: LinearGradient(
                colors: gradientColors
                    .map((color) => color.withOpacity(0.3))
                    .toList(),
              ),
            ),
          ),
        ],
        lineTouchData: LineTouchData(
          touchTooltipData: LineTouchTooltipData(
            getTooltipItems: (touchedSpots) {
              return touchedSpots.map((spot) {
                return LineTooltipItem(
                  "${spot.y.toInt()}",
                  const TextStyle(
                    color: Colors.black, // Tooltip text color
                    fontWeight: FontWeight.bold,
                  ),
                );
              }).toList();
            },
          ),
          getTouchedSpotIndicator: (barData, spotIndexes) {
            return spotIndexes.map((index) {
              return TouchedSpotIndicatorData(
                FlLine(
                  color: gradientColors.last, // Match line color
                  strokeWidth: 4,
                ),
                FlDotData(
                  show: true,
                  getDotPainter: (spot, percent, barData, index) {
                    return FlDotCirclePainter(
                      radius: 4,
                      color: gradientColors.last,
                      strokeWidth: 2,
                      strokeColor: Colors.white,
                    );
                  },
                ),
              );
            }).toList();
          },
        ));
  }
}
