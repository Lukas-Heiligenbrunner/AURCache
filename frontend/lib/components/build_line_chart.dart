import 'package:fl_chart/fl_chart.dart';
import 'package:flutter/material.dart';

class BuildLineChart extends StatefulWidget {
  const BuildLineChart({super.key});

  @override
  State<BuildLineChart> createState() => _BuildLineChartState();
}

class _BuildLineChartState extends State<BuildLineChart> {
  List<Color> gradientColors = [
    Colors.purple,
    Colors.deepPurpleAccent,
  ];

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
    Widget text;
    switch (value) {
      case 0:
        text = const Text('Jul', style: style);
        break;
      case 2:
        text = const Text('Aug', style: style);
        break;
      case 4:
        text = const Text('Sep', style: style);
        break;
      case 6:
        text = const Text('Oct', style: style);
        break;
      case 8:
        text = const Text('Nov', style: style);
        break;
      case 10:
        text = const Text('Dec', style: style);
        break;
      default:
        text = const Text('', style: style);
        break;
    }

    return SideTitleWidget(
      axisSide: meta.axisSide,
      child: text,
    );
  }

  Widget leftTitleWidgets(double value, TitleMeta meta) {
    const style = TextStyle(
      fontWeight: FontWeight.bold,
      fontSize: 12,
    );
    String text;
    switch (value.toInt()) {
      case 0:
        text = '10K';
        break;
      case 3:
        text = '30k';
        break;
      case 5:
        text = '50k';
        break;
      default:
        return Container();
    }

    return Text(text, style: style, textAlign: TextAlign.left);
  }

  LineChartData mainData() {
    return LineChartData(
      gridData: FlGridData(
        show: true,
        drawVerticalLine: false,
        horizontalInterval: 1,
        verticalInterval: 1,
        getDrawingHorizontalLine: (value) {
          return FlLine(
              color: Colors.grey.withOpacity(0.8),
              strokeWidth: 1,
              dashArray: [5, 5]);
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
      maxX: 10.25,
      minY: 0,
      maxY: 6,
      lineBarsData: [
        LineChartBarData(
          spots: const [
            FlSpot(0, 3),
            FlSpot(1, 1),
            FlSpot(2, 2),
            FlSpot(3, 1),
            FlSpot(4, 4),
            FlSpot(5, 2.5),
            FlSpot(6, 1.7),
            FlSpot(7, 2.1),
            FlSpot(8, 3),
            FlSpot(9, 4),
            FlSpot(10, 3),
          ],
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
              // limitate which dot to sho
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
    );
  }
}
