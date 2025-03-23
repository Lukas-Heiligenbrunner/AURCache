// GENERATED CODE - DO NOT MODIFY BY HAND

part of 'packages.dart';

// **************************************************************************
// RiverpodGenerator
// **************************************************************************

String _$listPackagesHash() => r'913d188d8211bc2473f46be7f830d7c29d2615bf';

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

/// See also [listPackages].
@ProviderFor(listPackages)
const listPackagesProvider = ListPackagesFamily();

/// See also [listPackages].
class ListPackagesFamily extends Family<AsyncValue<List<SimplePackage>>> {
  /// See also [listPackages].
  const ListPackagesFamily();

  /// See also [listPackages].
  ListPackagesProvider call({
    int? limit,
  }) {
    return ListPackagesProvider(
      limit: limit,
    );
  }

  @override
  ListPackagesProvider getProviderOverride(
    covariant ListPackagesProvider provider,
  ) {
    return call(
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
  String? get name => r'listPackagesProvider';
}

/// See also [listPackages].
class ListPackagesProvider
    extends AutoDisposeFutureProvider<List<SimplePackage>> {
  /// See also [listPackages].
  ListPackagesProvider({
    int? limit,
  }) : this._internal(
          (ref) => listPackages(
            ref as ListPackagesRef,
            limit: limit,
          ),
          from: listPackagesProvider,
          name: r'listPackagesProvider',
          debugGetCreateSourceHash:
              const bool.fromEnvironment('dart.vm.product')
                  ? null
                  : _$listPackagesHash,
          dependencies: ListPackagesFamily._dependencies,
          allTransitiveDependencies:
              ListPackagesFamily._allTransitiveDependencies,
          limit: limit,
        );

  ListPackagesProvider._internal(
    super._createNotifier, {
    required super.name,
    required super.dependencies,
    required super.allTransitiveDependencies,
    required super.debugGetCreateSourceHash,
    required super.from,
    required this.limit,
  }) : super.internal();

  final int? limit;

  @override
  Override overrideWith(
    FutureOr<List<SimplePackage>> Function(ListPackagesRef provider) create,
  ) {
    return ProviderOverride(
      origin: this,
      override: ListPackagesProvider._internal(
        (ref) => create(ref as ListPackagesRef),
        from: from,
        name: null,
        dependencies: null,
        allTransitiveDependencies: null,
        debugGetCreateSourceHash: null,
        limit: limit,
      ),
    );
  }

  @override
  AutoDisposeFutureProviderElement<List<SimplePackage>> createElement() {
    return _ListPackagesProviderElement(this);
  }

  @override
  bool operator ==(Object other) {
    return other is ListPackagesProvider && other.limit == limit;
  }

  @override
  int get hashCode {
    var hash = _SystemHash.combine(0, runtimeType.hashCode);
    hash = _SystemHash.combine(hash, limit.hashCode);

    return _SystemHash.finish(hash);
  }
}

@Deprecated('Will be removed in 3.0. Use Ref instead')
// ignore: unused_element
mixin ListPackagesRef on AutoDisposeFutureProviderRef<List<SimplePackage>> {
  /// The parameter `limit` of this provider.
  int? get limit;
}

class _ListPackagesProviderElement
    extends AutoDisposeFutureProviderElement<List<SimplePackage>>
    with ListPackagesRef {
  _ListPackagesProviderElement(super.provider);

  @override
  int? get limit => (origin as ListPackagesProvider).limit;
}

String _$getPackageHash() => r'4c36c7a06d84eb975eac55376439251cb78c2b65';

/// See also [getPackage].
@ProviderFor(getPackage)
const getPackageProvider = GetPackageFamily();

/// See also [getPackage].
class GetPackageFamily extends Family<AsyncValue<ExtendedPackage>> {
  /// See also [getPackage].
  const GetPackageFamily();

  /// See also [getPackage].
  GetPackageProvider call(
    int id,
  ) {
    return GetPackageProvider(
      id,
    );
  }

  @override
  GetPackageProvider getProviderOverride(
    covariant GetPackageProvider provider,
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
  String? get name => r'getPackageProvider';
}

/// See also [getPackage].
class GetPackageProvider extends AutoDisposeFutureProvider<ExtendedPackage> {
  /// See also [getPackage].
  GetPackageProvider(
    int id,
  ) : this._internal(
          (ref) => getPackage(
            ref as GetPackageRef,
            id,
          ),
          from: getPackageProvider,
          name: r'getPackageProvider',
          debugGetCreateSourceHash:
              const bool.fromEnvironment('dart.vm.product')
                  ? null
                  : _$getPackageHash,
          dependencies: GetPackageFamily._dependencies,
          allTransitiveDependencies:
              GetPackageFamily._allTransitiveDependencies,
          id: id,
        );

  GetPackageProvider._internal(
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
    FutureOr<ExtendedPackage> Function(GetPackageRef provider) create,
  ) {
    return ProviderOverride(
      origin: this,
      override: GetPackageProvider._internal(
        (ref) => create(ref as GetPackageRef),
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
  AutoDisposeFutureProviderElement<ExtendedPackage> createElement() {
    return _GetPackageProviderElement(this);
  }

  @override
  bool operator ==(Object other) {
    return other is GetPackageProvider && other.id == id;
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
mixin GetPackageRef on AutoDisposeFutureProviderRef<ExtendedPackage> {
  /// The parameter `id` of this provider.
  int get id;
}

class _GetPackageProviderElement
    extends AutoDisposeFutureProviderElement<ExtendedPackage>
    with GetPackageRef {
  _GetPackageProviderElement(super.provider);

  @override
  int get id => (origin as GetPackageProvider).id;
}
// ignore_for_file: type=lint
// ignore_for_file: subtype_of_sealed_class, invalid_use_of_internal_member, invalid_use_of_visible_for_testing_member, deprecated_member_use_from_same_package
