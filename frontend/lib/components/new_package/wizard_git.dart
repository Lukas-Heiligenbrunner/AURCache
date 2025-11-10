import 'package:flutter/material.dart';

class GitWizard extends StatefulWidget {
  const GitWizard({super.key, required this.onChange});
  final void Function((String, String, String)) onChange;

  @override
  State<GitWizard> createState() => _GitWizardState();
}

class _GitWizardState extends State<GitWizard> {
  final _formKey = GlobalKey<FormState>();

  final _repoUrlController = TextEditingController();
  final _subfolderController = TextEditingController();
  final _gitRefController = TextEditingController(text: "master");

  @override
  void dispose() {
    _repoUrlController.dispose();
    _subfolderController.dispose();
    _gitRefController.dispose();
    super.dispose();
  }

  void _onTextChanged(String _) {
    if (_formKey.currentState!.validate()) {
      final repoUrl = _repoUrlController.text.trim();
      final subfolder = _subfolderController.text.trim();
      final gitRef = _gitRefController.text.trim().isEmpty
          ? "master"
          : _gitRefController.text.trim();

      widget.onChange((repoUrl, gitRef, subfolder));
    }
  }

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.all(16.0),
      child: Form(
        key: _formKey,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            TextFormField(
              controller: _repoUrlController,
              decoration: const InputDecoration(
                labelText: 'Git Repo URL',
                hintText: 'https://github.com/user/repo.git',
                border: OutlineInputBorder(),
              ),
              onChanged: this._onTextChanged,
              validator: (value) {
                if (value == null || value.trim().isEmpty) {
                  return 'Repository URL is required';
                }
                if (!value.startsWith('http')) {
                  return 'Please enter a valid URL';
                }
                return null;
              },
            ),
            const SizedBox(height: 16),
            TextFormField(
              controller: _subfolderController,
              decoration: const InputDecoration(
                labelText: 'Subfolder (optional)',
                hintText: 'use root dir',
                border: OutlineInputBorder(),
              ),
              onChanged: this._onTextChanged,
            ),
            const SizedBox(height: 16),
            TextFormField(
              controller: _gitRefController,
              decoration: const InputDecoration(
                labelText: 'Git Ref (optional)',
                hintText: 'master',
                border: OutlineInputBorder(),
              ),
              onChanged: this._onTextChanged,
            ),
          ],
        ),
      ),
    );
  }
}
