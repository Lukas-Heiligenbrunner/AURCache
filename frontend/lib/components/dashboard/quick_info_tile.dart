import 'package:aurcache/models/quick_info_data.dart';
import 'package:flutter/material.dart';

import '../../constants/color_constants.dart';

class QuickInfoTile extends StatefulWidget {
  const QuickInfoTile({Key? key, required this.data}) : super(key: key);

  final QuickInfoData data;

  @override
  _QuickInfoTileState createState() => _QuickInfoTileState();
}

class _QuickInfoTileState extends State<QuickInfoTile> {
  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.all(defaultPadding),
      decoration: const BoxDecoration(
        color: secondaryColor,
        borderRadius: BorderRadius.all(Radius.circular(10)),
      ),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.center,
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        children: [
          Container(
            padding: const EdgeInsets.all(defaultPadding * 0.75),
            height: 64,
            width: 64,
            decoration: BoxDecoration(
              color: widget.data.color.withOpacity(0.1),
              borderRadius: const BorderRadius.all(Radius.circular(10)),
            ),
            child: Icon(
              widget.data.icon,
              color: widget.data.color,
              size: 32,
            ),
          ),
          Column(
            mainAxisAlignment: MainAxisAlignment.spaceEvenly,
            crossAxisAlignment: CrossAxisAlignment.end,
            children: [
              Text(
                widget.data.title,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: Theme.of(context).textTheme.titleSmall,
              ),
              Text(
                widget.data.subtitle,
                style: Theme.of(context)
                    .textTheme
                    .titleLarge!
                    .copyWith(color: Colors.white70),
              ),
            ],
          ),
        ],
      ),
    );
  }
}
