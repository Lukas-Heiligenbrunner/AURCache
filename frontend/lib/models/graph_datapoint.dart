class GraphDataPoint {
  final int month, year, count;

  factory GraphDataPoint.fromJson(Map<String, dynamic> json) {
    return GraphDataPoint(
      month: json["month"] as int,
      year: json["year"] as int,
      count: json["count"] as int,
    );
  }

  GraphDataPoint({
    required this.month,
    required this.year,
    required this.count,
  });
}
