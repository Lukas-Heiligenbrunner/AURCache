// GENERATED CODE - DO NOT MODIFY BY HAND

part of 'builds.dart';

// **************************************************************************
// RiverpodGenerator
// **************************************************************************

String _$listAllBuildsHash() => r'e0db016318c300dc724502c2837ba1c6195a1d57';

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

/// See also [listAllBuilds].
@ProviderFor(listAllBuilds)
const listAllBuildsProvider = ListAllBuildsFamily();

/// See also [listAllBuilds].
class ListAllBuildsFamily extends Family<AsyncValue<List<Build>>> {
  /// See also [listAllBuilds].
  const ListAllBuildsFamily();

  /// See also [listAllBuilds].
  ListAllBuildsProvider call({
    int? pkgID,
    int? limit,
  }) {
    return ListAllBuildsProvider(
      pkgID: pkgID,
      limit: limit,
    );
  }

  @override
  ListAllBuildsProvider getProviderOverride(
    covariant ListAllBuildsProvider provider,
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
  String? get name => r'listAllBuildsProvider';
}

/// See also [listAllBuilds].
class ListAllBuildsProvider extends AutoDisposeFutureProvider<List<Build>> {
  /// See also [listAllBuilds].
  ListAllBuildsProvider({
    int? pkgID,
    int? limit,
  }) : this._internal(
          (ref) => listAllBuilds(
            ref as ListAllBuildsRef,
            pkgID: pkgID,
            limit: limit,
          ),
          from: listAllBuildsProvider,
          name: r'listAllBuildsProvider',
          debugGetCreateSourceHash:
              const bool.fromEnvironment('dart.vm.product')
                  ? null
                  : _$listAllBuildsHash,
          dependencies: ListAllBuildsFamily._dependencies,
          allTransitiveDependencies:
              ListAllBuildsFamily._allTransitiveDependencies,
          pkgID: pkgID,
          limit: limit,
        );

  ListAllBuildsProvider._internal(
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
    FutureOr<List<Build>> Function(ListAllBuildsRef provider) create,
  ) {
    return ProviderOverride(
      origin: this,
      override: ListAllBuildsProvider._internal(
        (ref) => create(ref as ListAllBuildsRef),
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
  AutoDisposeFutureProviderElement<List<Build>> createElement() {
    return _ListAllBuildsProviderElement(this);
  }

  @override
  bool operator ==(Object other) {
    return other is ListAllBuildsProvider &&
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
mixin ListAllBuildsRef on AutoDisposeFutureProviderRef<List<Build>> {
  /// The parameter `pkgID` of this provider.
  int? get pkgID;

  /// The parameter `limit` of this provider.
  int? get limit;
}

class _ListAllBuildsProviderElement
    extends AutoDisposeFutureProviderElement<List<Build>>
    with ListAllBuildsRef {
  _ListAllBuildsProviderElement(super.provider);

  @override
  int? get pkgID => (origin as ListAllBuildsProvider).pkgID;
  @override
  int? get limit => (origin as ListAllBuildsProvider).limit;
}

String _$getBuildHash() => r'3dbf961ceefd1ab288478817d918ee2ee0bd1b84';

/// See also [getBuild].
@ProviderFor(getBuild)
const getBuildProvider = GetBuildFamily();

/// See also [getBuild].
class GetBuildFamily extends Family<AsyncValue<Build>> {
  /// See also [getBuild].
  const GetBuildFamily();

  /// See also [getBuild].
  GetBuildProvider call(
    int id,
  ) {
    return GetBuildProvider(
      id,
    );
  }

  @override
  GetBuildProvider getProviderOverride(
    covariant GetBuildProvider provider,
  ) {
    return call(
      provider.id,
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
  String? get name => r'getBuildProvider';
}

/// See also [getBuild].
class GetBuildProvider extends AutoDisposeFutureProvider<Build> {
  /// See also [getBuild].
  GetBuildProvider(
    int id,
  ) : this._internal(
          (ref) => getBuild(
            ref as GetBuildRef,
            id,
          ),
          from: getBuildProvider,
          name: r'getBuildProvider',
          debugGetCreateSourceHash:
              const bool.fromEnvironment('dart.vm.product')
                  ? null
                  : _$getBuildHash,
          dependencies: GetBuildFamily._dependencies,
          allTransitiveDependencies: GetBuildFamily._allTransitiveDependencies,
          id: id,
        );

  GetBuildProvider._internal(
    super._createNotifier, {
    required super.name,
    required super.dependencies,
    required super.allTransitiveDependencies,
    required super.debugGetCreateSourceHash,
    required super.from,
    required this.id,
  }) : super.internal();

  final int id;

  @override
  Override overrideWith(
    FutureOr<Build> Function(GetBuildRef provider) create,
  ) {
    return ProviderOverride(
      origin: this,
      override: GetBuildProvider._internal(
        (ref) => create(ref as GetBuildRef),
        from: from,
        name: null,
        dependencies: null,
        allTransitiveDependencies: null,
        debugGetCreateSourceHash: null,
        id: id,
      ),
    );
  }

  @override
  AutoDisposeFutureProviderElement<Build> createElement() {
    return _GetBuildProviderElement(this);
  }

  @override
  bool operator ==(Object other) {
    return other is GetBuildProvider && other.id == id;
  }

  @override
  int get hashCode {
    var hash = _SystemHash.combine(0, runtimeType.hashCode);
    hash = _SystemHash.combine(hash, id.hashCode);

    return _SystemHash.finish(hash);
  }
}

@Deprecated('Will be removed in 3.0. Use Ref instead')
// ignore: unused_element
mixin GetBuildRef on AutoDisposeFutureProviderRef<Build> {
  /// The parameter `id` of this provider.
  int get id;
}

class _GetBuildProviderElement extends AutoDisposeFutureProviderElement<Build>
    with GetBuildRef {
  _GetBuildProviderElement(super.provider);

  @override
  int get id => (origin as GetBuildProvider).id;
}
// ignore_for_file: type=lint
// ignore_for_file: subtype_of_sealed_class, invalid_use_of_internal_member, invalid_use_of_visible_for_testing_member, deprecated_member_use_from_same_package
