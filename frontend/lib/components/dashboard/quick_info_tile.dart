import 'package:aurcache/components/dashboard/tile_container.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';

class QuickInfoTile extends StatefulWidget {
  const QuickInfoTile(
      {super.key,
      required this.icon,
      required this.title,
      required this.value,
      this.trendPercent});

  final Widget icon;
  final String title, value;
  final double? trendPercent;

  @override
  _QuickInfoTileState createState() => _QuickInfoTileState();
}

class _QuickInfoTileState extends State<QuickInfoTile> {
  @override
  Widget build(BuildContext context) {
    return Tilecontainer(
        child: Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisAlignment: MainAxisAlignment.spaceBetween,
      children: [
        Text(
          widget.title,
          style: TextStyle(fontSize: 16, color: Colors.white.withOpacity(0.8)),
        ),
        if (context.desktop)
          SizedBox(
            height: 8,
          ),
        Row(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: [
            Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Padding(
                  padding: const EdgeInsets.only(left: 0),
                  child: Text(
                    widget.value,
                    style: TextStyle(
                        fontSize: context.desktop ? 30 : 24,
                        fontWeight: FontWeight.bold,
                        color: Colors.white),
                  ),
                ),
                if (context.desktop)
                  SizedBox(
                    height: 8,
                  ),
                _buildTrend(),
              ],
            ),
            SizedBox(
              height: context.desktop ? 64 : 42,
              child: widget.icon,
            )
          ],
        )
      ],
    ));
  }

  Widget _buildTrend() {
    if (widget.trendPercent == null) {
      return Row();
    }

    if (widget.trendPercent! > .0) {
      return Row(
        children: [
          Icon(
            Icons.keyboard_double_arrow_up_outlined,
            color: Color(0xffA9FF0F),
          ),
          Text(
            "${widget.trendPercent?.abs().toStringAsFixed(1) ?? 42}%",
            style: TextStyle(
              color: Color(0xffA9FF0F),
            ),
          )
        ],
      );
    } else if (widget.trendPercent! > .0) {
      return Row(
        children: [
          Icon(
            Icons.keyboard_double_arrow_down_outlined,
            color: Color(0xffFF4752),
          ),
          Text(
            "${widget.trendPercent?.abs().toStringAsFixed(1) ?? 42}%",
            style: TextStyle(color: Color(0xffFF4752)),
          )
        ],
      );
    } else {
      return Row(
        children: [
          Icon(
            Icons.keyboard_double_arrow_right,
            color: Color(0xff868686),
          ),
          Text(
            "${widget.trendPercent?.abs().toStringAsFixed(1) ?? 42}%",
            style: TextStyle(color: Color(0xff868686)),
          )
        ],
      );
    }
  }
}
