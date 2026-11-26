.DEFAULT_GOAL := release

override host_os := $(shell uname -s)
ifeq ($(host_os),Darwin)
ifeq ($(shell /usr/sbin/sysctl -in sysctl.proc_translated 2>/dev/null),1)
$(error Run make from a native macOS shell)
endif
override package_platform := macos
else ifeq ($(host_os),Linux)
ifneq ($(filter release dev,$(or $(MAKECMDGOALS),$(.DEFAULT_GOAL))),)
override package_platform := $(shell ID=; if test -r /etc/os-release; then . /etc/os-release && printf '%s' "$$ID"; fi)
ifeq ($(filter fedora ubuntu,$(package_platform)),)
$(error Unsupported Linux distribution '$(package_platform)' (expected Fedora or Ubuntu))
endif
endif
else
$(error Unsupported operating system '$(host_os)' (expected macOS, Fedora, or Ubuntu))
endif

override host_arch := $(shell uname -m)
ifneq ($(filter arm64 aarch64,$(host_arch)),)
override host_arch := aarch64
else ifneq ($(host_arch),x86_64)
$(error Unsupported host architecture '$(host_arch)' (expected x86_64 or ARM64))
endif

.PHONY: release dev install install-release install-dev

release dev:
	@sh packaging/package.sh build $@ $(package_platform) $(host_arch)

install: install-release

install-release install-dev:
	@sh packaging/package.sh install $(@:install-%=%) $(if $(filter Linux,$(host_os)),linux,$(package_platform)) $(host_arch)
