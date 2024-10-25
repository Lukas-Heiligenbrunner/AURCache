---
sidebar_position: 4
---

# Cross-platform Builds

Cross-platform builds are a bit tricky to setup, especially inside virtualizations such as lxc containers.
This is still a bit work in progress, so expect some bugs.

## How it works
This feature depends on Qemu binfmt_misc support (QEMU user space emulation)(https://www.qemu.org/docs/master/user/main.html). 

For every build a new buildcontainer is spawned, either on docker host or inside the aurcache-container with the selected target platform. 
Docker uses binfmt_misc to register the qemu interpreter for the target platform. This should happen automatically when the container is started.

## Supported platforms
* x86_64 (default)
* aarch64
* armv7

Those platforms are limited to those 3 for now, because the archlinux baseimage is only available for those platforms.
Which is required for the build container.

## Limitations
qemu-binfmt only supports x86_64. So this can only be used to cross compile from an x86_64 host to other platforms.

## Troubleshooting

If your output looks like this:
```
Pulling image: ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest
{"msg":"exec container process (missing dynamic library?) `/usr/sbin/sh`: No such file or directory","level":"error","time":"2024-10-25T19:56:14.842412Z"}
Docker container wait error
```
You have misconfigured qemu-binfmt. 

You might have a look at: https://dshcherb.github.io/2017/12/04/qemu-kvm-virtual-machines-in-unprivileged-lxd.html

`binfmt_misc` should be enabled in the kernel or as a kernel module. You can check this by running `cat /proc/sys/fs/binfmt_misc/status`. 
If it is not enabled, you can enable it by running `mount binfmt_misc -t binfmt_misc /proc/sys/fs/binfmt_misc`.