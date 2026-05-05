import 'package:aurcache/api/API.dart';
import 'package:aurcache/api/settings.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:toastification/toastification.dart';

import '../models/settings.dart';

class ConfigFilesScreen extends StatefulWidget {
  const ConfigFilesScreen({super.key, this.pkgid, this.packageName});

  /// When non-null, edits per-package overrides; reset reverts to the global
  /// value. When null, edits the global rows.
  final int? pkgid;

  /// Optional name to render in the AppBar for context.
  final String? packageName;

  @override
  State<ConfigFilesScreen> createState() => _ConfigFilesScreenState();
}

class _ConfigFilesScreenState extends State<ConfigFilesScreen>
    with SingleTickerProviderStateMixin {
  late final TabController _tabController;

  final _makepkgController = TextEditingController();
  final _pacmanController = TextEditingController();

  SingleSetting? _makepkg;
  SingleSetting? _pacman;

  bool _loading = true;
  bool _makepkgDirty = false;
  bool _pacmanDirty = false;

  bool get _isPackageScope => widget.pkgid != null;

  @override
  void initState() {
    super.initState();
    _tabController = TabController(length: 2, vsync: this);
    _loadFiles();
  }

  @override
  void dispose() {
    _tabController.dispose();
    _makepkgController.dispose();
    _pacmanController.dispose();
    super.dispose();
  }

  Future<void> _loadFiles() async {
    setState(() => _loading = true);
    try {
      final makepkg = await API.getSetting('makepkg_conf', pkgid: widget.pkgid);
      final pacman = await API.getSetting('pacman_conf', pkgid: widget.pkgid);
      _makepkgController.text = makepkg.value;
      _pacmanController.text = pacman.value;
      setState(() {
        _makepkg = makepkg;
        _pacman = pacman;
        _makepkgDirty = false;
        _pacmanDirty = false;
      });
    } catch (e) {
      _showToast('Failed to load config files: $e', success: false);
    } finally {
      if (mounted) setState(() => _loading = false);
    }
  }

  Future<void> _save(String key) async {
    final controller = key == 'makepkg_conf'
        ? _makepkgController
        : _pacmanController;
    final filename = key == 'makepkg_conf' ? 'makepkg.conf' : 'pacman.conf';

    final success = await API.patchSetting(
      key,
      controller.text,
      pkgid: widget.pkgid,
    );
    _showToast(
      success ? 'Saved $filename' : 'Failed to save $filename',
      success: success,
    );
    if (success) await _loadFiles();
  }

  Future<void> _resetOverride(String key) async {
    final filename = key == 'makepkg_conf' ? 'makepkg.conf' : 'pacman.conf';
    final what = _isPackageScope
        ? 'package override and use the global value'
        : 'stored override and use the builder default';
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text('Reset $filename?'),
        content: Text('Discard the $what for $filename.'),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(ctx).pop(true),
            child: const Text('Reset'),
          ),
        ],
      ),
    );
    if (confirmed != true) return;

    final ok = await API.resetSetting(key, pkgid: widget.pkgid);
    _showToast(
      ok ? 'Reset $filename' : 'Failed to reset $filename',
      success: ok,
    );
    if (ok) await _loadFiles();
  }

  void _showToast(String message, {required bool success}) {
    toastification.show(
      title: Text(message),
      autoCloseDuration: const Duration(seconds: 4),
      type: success ? ToastificationType.success : ToastificationType.error,
    );
  }

  String? _sourceBadge(SingleSetting? entry) {
    if (entry == null) return null;
    if (entry.envForced && !_isPackageScope) return 'env-forced';
    if (entry.isDefault) return 'default';
    if (_isPackageScope && entry.isInherited) return 'inherited from global';
    if (_isPackageScope && entry.envForced) return 'inherited from env';
    return null;
  }

  @override
  Widget build(BuildContext context) {
    final backRoute = _isPackageScope
        ? '/package/${widget.pkgid}/settings'
        : '/settings';
    final title = _isPackageScope
        ? widget.packageName == null
              ? 'Package Config Files'
              : '${widget.packageName}: Config Files'
        : 'Config Files';

    return Scaffold(
      appBar: AppBar(
        leading: context.mobile && !_isPackageScope
            ? IconButton(
                icon: const Icon(Icons.menu),
                onPressed: () => Scaffold.of(context).openDrawer(),
              )
            : IconButton(
                icon: const Icon(Icons.arrow_back),
                onPressed: () => context.go(backRoute),
              ),
        title: Text(title),
        bottom: TabBar(
          controller: _tabController,
          tabs: [
            Tab(
              child: _tabLabel(
                'makepkg.conf',
                _makepkgDirty,
                _sourceBadge(_makepkg),
              ),
            ),
            Tab(
              child: _tabLabel(
                'pacman.conf',
                _pacmanDirty,
                _sourceBadge(_pacman),
              ),
            ),
          ],
        ),
      ),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : TabBarView(
              controller: _tabController,
              children: [
                _buildEditor(
                  controller: _makepkgController,
                  entry: _makepkg,
                  storageKey: 'makepkg_conf',
                  hint: _isPackageScope
                      ? 'Per-package makepkg.conf override. Falls back to the global value when empty / reset. PKGDEST and MAKEFLAGS are always appended automatically.'
                      : 'Custom makepkg.conf — PKGDEST and MAKEFLAGS are always appended automatically.',
                  onDirty: () => setState(() => _makepkgDirty = true),
                ),
                _buildEditor(
                  controller: _pacmanController,
                  entry: _pacman,
                  storageKey: 'pacman_conf',
                  hint: _isPackageScope
                      ? 'Per-package pacman.conf override. Replaces /etc/pacman.conf inside this package\'s build container. Reset to fall back to the global value.'
                      : 'Custom pacman.conf — replaces /etc/pacman.conf in the build container. Leave empty to use the builder default.',
                  onDirty: () => setState(() => _pacmanDirty = true),
                ),
              ],
            ),
    );
  }

  Widget _tabLabel(String text, bool dirty, String? sourceBadge) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(text),
        if (sourceBadge != null) ...[
          const SizedBox(width: 6),
          Text(
            '($sourceBadge)',
            style: const TextStyle(
              color: Colors.grey,
              fontStyle: FontStyle.italic,
              fontSize: 12,
            ),
          ),
        ],
        if (dirty) ...[
          const SizedBox(width: 6),
          const Icon(Icons.circle, size: 8, color: Colors.orange),
        ],
      ],
    );
  }

  Widget _buildEditor({
    required TextEditingController controller,
    required SingleSetting? entry,
    required String storageKey,
    required String hint,
    required VoidCallback onDirty,
  }) {
    final hasOverride = _isPackageScope
        ? entry?.isPackageOverride ?? false
        : (entry != null &&
              !entry.envForced &&
              !entry.isDefault &&
              !entry.isInherited);
    // Env locks the editor only on the global page. On a pkg page a per-pkg
    // override beats env, so the user can still write a value here.
    final envLocking = (entry?.envForced ?? false) && !_isPackageScope;

    return Padding(
      padding: const EdgeInsets.all(16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(hint, style: Theme.of(context).textTheme.bodySmall),
          if (envLocking) ...[
            const SizedBox(height: 6),
            const Text(
              'Forced by environment variable — read-only.',
              style: TextStyle(color: Colors.red),
            ),
          ],
          const SizedBox(height: 12),
          Expanded(
            child: TextField(
              controller: controller,
              enabled: !envLocking,
              onChanged: (_) => onDirty(),
              maxLines: null,
              expands: true,
              textAlignVertical: TextAlignVertical.top,
              style: const TextStyle(fontFamily: 'monospace', fontSize: 13),
              decoration: const InputDecoration(
                border: OutlineInputBorder(),
                contentPadding: EdgeInsets.all(12),
              ),
            ),
          ),
          const SizedBox(height: 12),
          Wrap(
            alignment: WrapAlignment.end,
            spacing: 12,
            runSpacing: 8,
            children: [
              if (hasOverride)
                OutlinedButton.icon(
                  onPressed: () => _resetOverride(storageKey),
                  icon: const Icon(Icons.restart_alt),
                  label: Text(
                    _isPackageScope ? 'Reset to inherited' : 'Reset to default',
                  ),
                ),
              OutlinedButton.icon(
                onPressed: _loadFiles,
                icon: const Icon(Icons.refresh),
                label: const Text('Reload'),
              ),
              FilledButton.icon(
                onPressed: envLocking ? null : () => _save(storageKey),
                icon: const Icon(Icons.save),
                label: const Text('Save'),
              ),
            ],
          ),
        ],
      ),
    );
  }
}
