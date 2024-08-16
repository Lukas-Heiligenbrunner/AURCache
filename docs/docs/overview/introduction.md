---
sidebar_position: 1
---

# Introduction

AURCache is a build server and repository for Archlinux packages sourced from the AUR (Arch User Repository). It features a Flutter frontend and Rust backend, enabling users to add packages for building and subsequently serves them as a pacman repository. Notably, AURCache automatically detects when a package is out of date and displays it within the frontend.


## Advantages

- **Avoid repeated builds**: Build your packages only once on your server and not on every client.
- **Reduce CPU and memory usage**: Clients only need to download packages.
- **Reduce build time**: Build packages in parallel.
- **Reduce network traffic**: Serve packages from your local network.
- **Automatically update packages**: AURCache automatically checks for updates. 
- **Customize your repository**: Add custom packages to your repository.

## Getting Started
Get started with [Quick Start](/docs/overview/quick-start) to try it out.

For advanced setup, see the [Configuration](/docs/configuration) page.