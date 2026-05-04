import 'package:aurcache/api/API.dart';
import 'package:aurcache/api/settings.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:toastification/toastification.dart';

class ConfigFilesScreen extends StatefulWidget {
  const ConfigFilesScreen({super.key});

  @override
  State<ConfigFilesScreen> createState() => _ConfigFilesScreenState();
}

class _ConfigFilesScreenState extends State<ConfigFilesScreen>
    with SingleTickerProviderStateMixin {
  late final TabController _tabController;

  final _makepkgController = TextEditingController();
  final _pacmanController = TextEditingController();

  bool _loading = true;
  bool _makepkgDirty = false;
  bool _pacmanDirty = false;

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
      final makepkg = await API.getSetting('makepkg_conf');
      final pacman = await API.getSetting('pacman_conf');
      _makepkgController.text = makepkg.value;
      _pacmanController.text = pacman.value;
      setState(() {
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

    final success = await API.patchSetting(key, controller.text);
    _showToast(
      success ? 'Saved $filename' : 'Failed to save $filename',
      success: success,
    );
    if (success) {
      setState(() {
        if (key == 'makepkg_conf') _makepkgDirty = false;
        if (key == 'pacman_conf') _pacmanDirty = false;
      });
    }
  }

  Future<void> _resetToDefault(String key) async {
    final filename = key == 'makepkg_conf' ? 'makepkg.conf' : 'pacman.conf';
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text('Reset $filename?'),
        content: Text(
          'Discard the stored override and use the builder default for $filename.',
        ),
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

    final ok = await API.resetSetting(key);
    _showToast(
      ok ? 'Reset $filename to default' : 'Failed to reset $filename',
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

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        leading: context.mobile
            ? IconButton(
                icon: const Icon(Icons.menu),
                onPressed: () => Scaffold.of(context).openDrawer(),
              )
            : IconButton(
                icon: const Icon(Icons.arrow_back),
                onPressed: () => context.go('/settings'),
              ),
        title: const Text('Config Files'),
        bottom: TabBar(
          controller: _tabController,
          tabs: [
            Tab(child: _tabLabel('makepkg.conf', _makepkgDirty)),
            Tab(child: _tabLabel('pacman.conf', _pacmanDirty)),
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
                  key: 'makepkg_conf',
                  hint:
                      'Custom makepkg.conf — PKGDEST and MAKEFLAGS are always appended automatically.',
                  onDirty: () => setState(() => _makepkgDirty = true),
                ),
                _buildEditor(
                  controller: _pacmanController,
                  key: 'pacman_conf',
                  hint:
                      'Custom pacman.conf — replaces /etc/pacman.conf in the build container. Leave empty to use the builder default.',
                  onDirty: () => setState(() => _pacmanDirty = true),
                ),
              ],
            ),
    );
  }

  Widget _tabLabel(String text, bool dirty) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(text),
        if (dirty) ...[
          const SizedBox(width: 6),
          const Icon(Icons.circle, size: 8, color: Colors.orange),
        ],
      ],
    );
  }

  Widget _buildEditor({
    required TextEditingController controller,
    required String key,
    required String hint,
    required VoidCallback onDirty,
  }) {
    return Padding(
      padding: const EdgeInsets.all(16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(hint, style: Theme.of(context).textTheme.bodySmall),
          const SizedBox(height: 12),
          Expanded(
            child: TextField(
              controller: controller,
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
          Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              OutlinedButton.icon(
                onPressed: () => _resetToDefault(key),
                icon: const Icon(Icons.restart_alt),
                label: const Text('Reset to default'),
              ),
              const SizedBox(width: 12),
              OutlinedButton.icon(
                onPressed: _loadFiles,
                icon: const Icon(Icons.refresh),
                label: const Text('Reload'),
              ),
              const SizedBox(width: 12),
              FilledButton.icon(
                onPressed: () => _save(key),
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
