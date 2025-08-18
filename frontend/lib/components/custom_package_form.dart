import 'package:aurcache/providers/packages.dart';
import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:toastification/toastification.dart';

import '../api/API.dart';
import '../constants/color_constants.dart';

class CustomPackageForm extends ConsumerStatefulWidget {
  const CustomPackageForm({super.key});

  @override
  ConsumerState<CustomPackageForm> createState() => _CustomPackageFormState();
}

class _CustomPackageFormState extends ConsumerState<CustomPackageForm> {
  final _formKey = GlobalKey<FormState>();
  final _nameController = TextEditingController();
  final _versionController = TextEditingController();
  final _pkgbuildController = TextEditingController();
  bool _isLoading = false;
  
  final List<String> _availableArchs = ['x86_64', 'aarch64'];
  List<String> _selectedArchs = ['x86_64'];

  @override
  void dispose() {
    _nameController.dispose();
    _versionController.dispose();
    _pkgbuildController.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Form(
      key: _formKey,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            "Upload Custom PKGBUILD",
            style: Theme.of(context).textTheme.titleMedium,
          ),
          const SizedBox(height: 16),
          
          Text(
            "Upload a custom PKGBUILD file to build your own packages.",
            style: Theme.of(context).textTheme.bodySmall?.copyWith(
              color: Colors.grey[400],
            ),
          ),
          const SizedBox(height: 24),

          // Package Name
          TextFormField(
            controller: _nameController,
            decoration: const InputDecoration(
              labelText: "Package Name",
              hintText: "Enter the package name",
              border: OutlineInputBorder(),
            ),
            validator: (value) {
              if (value == null || value.isEmpty) {
                return 'Please enter a package name';
              }
              if (!RegExp(r'^[a-z0-9][a-z0-9\-_]*$').hasMatch(value)) {
                return 'Package name must start with a letter/number and contain only lowercase letters, numbers, hyphens and underscores';
              }
              return null;
            },
          ),
          const SizedBox(height: 16),

          // Version
          TextFormField(
            controller: _versionController,
            decoration: const InputDecoration(
              labelText: "Version",
              hintText: "e.g., 1.0.0",
              border: OutlineInputBorder(),
            ),
            validator: (value) {
              if (value == null || value.isEmpty) {
                return 'Please enter a version';
              }
              return null;
            },
          ),
          const SizedBox(height: 16),

          // Architectures
          Text(
            "Target Architectures:",
            style: Theme.of(context).textTheme.bodyMedium,
          ),
          const SizedBox(height: 8),
          Wrap(
            spacing: 8,
            children: _availableArchs.map((arch) {
              return FilterChip(
                label: Text(arch),
                selected: _selectedArchs.contains(arch),
                onSelected: (selected) {
                  setState(() {
                    if (selected) {
                      _selectedArchs.add(arch);
                    } else {
                      _selectedArchs.remove(arch);
                    }
                  });
                },
                selectedColor: primaryColor.withOpacity(0.3),
                checkmarkColor: Colors.white,
              );
            }).toList(),
          ),
          const SizedBox(height: 16),

          // PKGBUILD Content
          Text(
            "PKGBUILD Content:",
            style: Theme.of(context).textTheme.bodyMedium,
          ),
          const SizedBox(height: 8),
          TextFormField(
            controller: _pkgbuildController,
            decoration: const InputDecoration(
              hintText: "Paste your PKGBUILD content here...",
              border: OutlineInputBorder(),
            ),
            maxLines: 15,
            style: TextStyle(fontFamily: 'monospace'),
            validator: (value) {
              if (value == null || value.isEmpty) {
                return 'Please enter PKGBUILD content';
              }
              if (!value.contains('pkgname=')) {
                return 'PKGBUILD must contain pkgname=';
              }
              return null;
            },
          ),
          const SizedBox(height: 24),

          // Submit Button
          SizedBox(
            width: double.infinity,
            child: ElevatedButton(
              onPressed: _isLoading ? null : _submitForm,
              style: ElevatedButton.styleFrom(
                backgroundColor: primaryColor,
                padding: const EdgeInsets.symmetric(vertical: 16),
              ),
              child: _isLoading
                  ? const SizedBox(
                      height: 20,
                      width: 20,
                      child: CircularProgressIndicator(strokeWidth: 2),
                    )
                  : const Text(
                      "Upload Package",
                      style: TextStyle(color: Colors.white),
                    ),
            ),
          ),
        ],
      ),
    );
  }

  Future<void> _submitForm() async {
    if (!_formKey.currentState!.validate()) {
      return;
    }

    if (_selectedArchs.isEmpty) {
      toastification.show(
        title: const Text('Please select at least one architecture'),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.error,
      );
      return;
    }

    setState(() {
      _isLoading = true;
    });

    try {
      await API.addCustomPackage(
        name: _nameController.text.trim(),
        version: _versionController.text.trim(),
        pkgbuildContent: _pkgbuildController.text,
        selectedArchs: _selectedArchs,
      );

      toastification.show(
        title: const Text('Custom package uploaded successfully!'),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.success,
      );

      // Clear form
      _nameController.clear();
      _versionController.clear();
      _pkgbuildController.clear();
      _selectedArchs = ['x86_64'];

      // Refresh packages list
      ref.invalidate(listPackagesProvider());

    } on DioException catch (e) {
      String message = 'Failed to upload package';
      if (e.response?.data != null) {
        message += ': ${e.response!.data}';
      }
      
      toastification.show(
        title: Text(message),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.error,
      );
    } catch (e) {
      toastification.show(
        title: Text('Unexpected error: $e'),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.error,
      );
    } finally {
      setState(() {
        _isLoading = false;
      });
    }
  }
}