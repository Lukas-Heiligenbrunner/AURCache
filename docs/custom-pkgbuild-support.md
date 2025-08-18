# Custom PKGBUILD Support

This document describes the custom PKGBUILD upload feature that allows users to upload and build their own PKGBUILD files in addition to AUR packages.

## Features

### Package Types
- **AUR Packages**: Traditional packages from the Arch User Repository
- **Custom Packages**: User-uploaded PKGBUILD files

### Backend API Endpoints

#### Upload Custom Package
```
POST /api/package/custom
Content-Type: application/json

{
  "name": "my-custom-package",
  "version": "1.0.0",
  "pkgbuild_content": "# PKGBUILD content here...",
  "platforms": ["x86_64"],
  "build_flags": ["-Syu", "--noconfirm"]
}
```

#### Update Custom Package
```
POST /api/package/{id}/update-custom
Content-Type: application/json

{
  "version": "1.0.1", 
  "pkgbuild_content": "# Updated PKGBUILD content here..."
}
```

### Database Schema

The `packages` table has been extended with:
- `package_type`: INTEGER (0=AUR, 1=Custom)
- `custom_pkgbuild_path`: TEXT (path to stored PKGBUILD file)

### Frontend UI Changes

#### Package List
- Package type badges showing "AUR" or "Custom"
- Color-coded: AUR packages in blue, Custom packages in green

#### AUR/Package Management Screen
- Converted to tabbed interface
- **AUR Packages** tab: Search and add AUR packages
- **Custom PKGBUILD** tab: Upload custom PKGBUILD files

#### Package Details
- Package type indicator in page header
- Conditional display of AUR-specific information
- Custom package update dialog for PKGBUILD modification
- Hide AUR links for custom packages

### Build Process

The builder automatically detects package type and:
- **AUR packages**: Uses `paru` to fetch and build from AUR
- **Custom packages**: Mounts local PKGBUILD file and uses `makepkg`

### File Storage

Custom PKGBUILD files are stored in:
```
./custom_packages/{package_name}/PKGBUILD
```

### Package Management

- **Delete**: Removes package from database and cleans up PKGBUILD files
- **Update**: For custom packages, allows updating version and PKGBUILD content
- **Rebuild**: Standard rebuild functionality works for both types

## Usage

1. Navigate to the AUR/Package Management page
2. Switch to the "Custom PKGBUILD" tab  
3. Fill in package details:
   - Package name (lowercase, alphanumeric with hyphens/underscores)
   - Version number
   - Target architectures
   - PKGBUILD content
4. Click "Upload Package" to submit
5. Package will be queued for building like any AUR package

## Notes

- Custom packages are clearly marked with type badges
- AUR-specific features (links, update checks) are hidden for custom packages
- Custom packages can be updated with new PKGBUILD content
- Deletion properly cleans up stored PKGBUILD files
- All existing package management features work with both types