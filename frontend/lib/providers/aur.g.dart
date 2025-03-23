// GENERATED CODE - DO NOT MODIFY BY HAND

part of 'aur.dart';

// **************************************************************************
// RiverpodGenerator
// **************************************************************************

String _$getAurPackagesHash() => r'2b92be0425e8ea0dcf3b85bce9dd99213e270e26';

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

/// See also [getAurPackages].
@ProviderFor(getAurPackages)
const getAurPackagesProvider = GetAurPackagesFamily();

/// See also [getAurPackages].
class GetAurPackagesFamily extends Family<AsyncValue<List<AurPackage>>> {
  /// See also [getAurPackages].
  const GetAurPackagesFamily();

  /// See also [getAurPackages].
  GetAurPackagesProvider call(
    String query,
  ) {
    return GetAurPackagesProvider(
      query,
    );
  }

  @override
  GetAurPackagesProvider getProviderOverride(
    covariant GetAurPackagesProvider provider,
  ) {
    return call(
      provider.query,
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
  String? get name => r'getAurPackagesProvider';
}

/// See also [getAurPackages].
class GetAurPackagesProvider
    extends AutoDisposeFutureProvider<List<AurPackage>> {
  /// See also [getAurPackages].
  GetAurPackagesProvider(
    String query,
  ) : this._internal(
          (ref) => getAurPackages(
            ref as GetAurPackagesRef,
            query,
          ),
          from: getAurPackagesProvider,
          name: r'getAurPackagesProvider',
          debugGetCreateSourceHash:
              const bool.fromEnvironment('dart.vm.product')
                  ? null
                  : _$getAurPackagesHash,
          dependencies: GetAurPackagesFamily._dependencies,
          allTransitiveDependencies:
              GetAurPackagesFamily._allTransitiveDependencies,
          query: query,
        );

  GetAurPackagesProvider._internal(
    super._createNotifier, {
    required super.name,
    required super.dependencies,
    required super.allTransitiveDependencies,
    required super.debugGetCreateSourceHash,
    required super.from,
    required this.query,
  }) : super.internal();

  final String query;

  @override
  Override overrideWith(
    FutureOr<List<AurPackage>> Function(GetAurPackagesRef provider) create,
  ) {
    return ProviderOverride(
      origin: this,
      override: GetAurPackagesProvider._internal(
        (ref) => create(ref as GetAurPackagesRef),
        from: from,
        name: null,
        dependencies: null,
        allTransitiveDependencies: null,
        debugGetCreateSourceHash: null,
        query: query,
      ),
    );
  }

  @override
  AutoDisposeFutureProviderElement<List<AurPackage>> createElement() {
    return _GetAurPackagesProviderElement(this);
  }

  @override
  bool operator ==(Object other) {
    return other is GetAurPackagesProvider && other.query == query;
  }

  @override
  int get hashCode {
    var hash = _SystemHash.combine(0, runtimeType.hashCode);
    hash = _SystemHash.combine(hash, query.hashCode);

    return _SystemHash.finish(hash);
  }
}

@Deprecated('Will be removed in 3.0. Use Ref instead')
// ignore: unused_element
mixin GetAurPackagesRef on AutoDisposeFutureProviderRef<List<AurPackage>> {
  /// The parameter `query` of this provider.
  String get query;
}

class _GetAurPackagesProviderElement
    extends AutoDisposeFutureProviderElement<List<AurPackage>>
    with GetAurPackagesRef {
  _GetAurPackagesProviderElement(super.provider);

  @override
  String get query => (origin as GetAurPackagesProvider).query;
}
// ignore_for_file: type=lint
// ignore_for_file: subtype_of_sealed_class, invalid_use_of_internal_member, invalid_use_of_visible_for_testing_member, deprecated_member_use_from_same_package
