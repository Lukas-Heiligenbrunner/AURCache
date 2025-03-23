// GENERATED CODE - DO NOT MODIFY BY HAND

part of 'activity_log.dart';

// **************************************************************************
// RiverpodGenerator
// **************************************************************************

String _$listActivitiesHash() => r'990aecac2bc9c1ca1a67233fa93be828dd6cda35';

/// Copied from Dart SDK
class _SystemHash {
  _SystemHash._();

  static int combine(int hash, int value) {
    // ignore: parameter_assignments
    hash = 0x1fffffff & (hash + value);
    // ignore: parameter_assignments
    hash = 0x1fffffff & (hash + ((0x0007ffff & hash) << 10));
    return hash ^ (hash >> 6);
  }

  static int finish(int hash) {
    // ignore: parameter_assignments
    hash = 0x1fffffff & (hash + ((0x03ffffff & hash) << 3));
    // ignore: parameter_assignments
    hash = hash ^ (hash >> 11);
    return 0x1fffffff & (hash + ((0x00003fff & hash) << 15));
  }
}

/// See also [listActivities].
@ProviderFor(listActivities)
const listActivitiesProvider = ListActivitiesFamily();

/// See also [listActivities].
class ListActivitiesFamily extends Family<AsyncValue<List<Activity>>> {
  /// See also [listActivities].
  const ListActivitiesFamily();

  /// See also [listActivities].
  ListActivitiesProvider call({
    int? pkgID,
    int? limit,
  }) {
    return ListActivitiesProvider(
      pkgID: pkgID,
      limit: limit,
    );
  }

  @override
  ListActivitiesProvider getProviderOverride(
    covariant ListActivitiesProvider provider,
  ) {
    return call(
      pkgID: provider.pkgID,
      limit: provider.limit,
    );
  }

  static const Iterable<ProviderOrFamily>? _dependencies = null;

  @override
  Iterable<ProviderOrFamily>? get dependencies => _dependencies;

  static const Iterable<ProviderOrFamily>? _allTransitiveDependencies = null;

  @override
  Iterable<ProviderOrFamily>? get allTransitiveDependencies =>
      _allTransitiveDependencies;

  @override
  String? get name => r'listActivitiesProvider';
}

/// See also [listActivities].
class ListActivitiesProvider extends AutoDisposeFutureProvider<List<Activity>> {
  /// See also [listActivities].
  ListActivitiesProvider({
    int? pkgID,
    int? limit,
  }) : this._internal(
          (ref) => listActivities(
            ref as ListActivitiesRef,
            pkgID: pkgID,
            limit: limit,
          ),
          from: listActivitiesProvider,
          name: r'listActivitiesProvider',
          debugGetCreateSourceHash:
              const bool.fromEnvironment('dart.vm.product')
                  ? null
                  : _$listActivitiesHash,
          dependencies: ListActivitiesFamily._dependencies,
          allTransitiveDependencies:
              ListActivitiesFamily._allTransitiveDependencies,
          pkgID: pkgID,
          limit: limit,
        );

  ListActivitiesProvider._internal(
    super._createNotifier, {
    required super.name,
    required super.dependencies,
    required super.allTransitiveDependencies,
    required super.debugGetCreateSourceHash,
    required super.from,
    required this.pkgID,
    required this.limit,
  }) : super.internal();

  final int? pkgID;
  final int? limit;

  @override
  Override overrideWith(
    FutureOr<List<Activity>> Function(ListActivitiesRef provider) create,
  ) {
    return ProviderOverride(
      origin: this,
      override: ListActivitiesProvider._internal(
        (ref) => create(ref as ListActivitiesRef),
        from: from,
        name: null,
        dependencies: null,
        allTransitiveDependencies: null,
        debugGetCreateSourceHash: null,
        pkgID: pkgID,
        limit: limit,
      ),
    );
  }

  @override
  AutoDisposeFutureProviderElement<List<Activity>> createElement() {
    return _ListActivitiesProviderElement(this);
  }

  @override
  bool operator ==(Object other) {
    return other is ListActivitiesProvider &&
        other.pkgID == pkgID &&
        other.limit == limit;
  }

  @override
  int get hashCode {
    var hash = _SystemHash.combine(0, runtimeType.hashCode);
    hash = _SystemHash.combine(hash, pkgID.hashCode);
    hash = _SystemHash.combine(hash, limit.hashCode);

    return _SystemHash.finish(hash);
  }
}

@Deprecated('Will be removed in 3.0. Use Ref instead')
// ignore: unused_element
mixin ListActivitiesRef on AutoDisposeFutureProviderRef<List<Activity>> {
  /// The parameter `pkgID` of this provider.
  int? get pkgID;

  /// The parameter `limit` of this provider.
  int? get limit;
}

class _ListActivitiesProviderElement
    extends AutoDisposeFutureProviderElement<List<Activity>>
    with ListActivitiesRef {
  _ListActivitiesProviderElement(super.provider);

  @override
  int? get pkgID => (origin as ListActivitiesProvider).pkgID;
  @override
  int? get limit => (origin as ListActivitiesProvider).limit;
}
// ignore_for_file: type=lint
// ignore_for_file: subtype_of_sealed_class, invalid_use_of_internal_member, invalid_use_of_visible_for_testing_member, deprecated_member_use_from_same_package
