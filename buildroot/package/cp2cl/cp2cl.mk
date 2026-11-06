################################################################################
#
# cp2cl
#
# The binary is not built here. Building it is `make` at the top of the
# repository, in the container under docker/, which is where the musl
# cross-toolchain, the vendored LIVI crates and the `lto = "fat"` profile
# already are. Buildroot's cargo infrastructure would have to reproduce all
# three, and would still be the slowest thing in the build.
#
# `make app` stages the binary as dist/app/cp2cl-linux-<arch>, flat, one file
# per target, and the one this build was for is installed. `ARCH` is Buildroot's
# own — `aarch64` here, `arm` for the board that builds as armv7 — so the name is
# spelled from it rather than configured a second time. The `local` site takes
# the whole of dist/app, which carries the other board's binary too; it is
# installed by name, so that copy is rsync traffic and nothing else. Nothing else
# from the repository is copied: `target/` and the images under dist/firmware/
# never enter that rsync.
#
################################################################################

# Read out of Cargo.toml rather than written here a second time: the copy that
# lived here said 0.9.1 while the crate said 0.11.0, and this number is what
# names the build directory and what the log prints on every package step.
CP2CL_VERSION = $(shell sed -n 's/^version = "//p' $(BR2_EXTERNAL_CP2CL_PATH)/../Cargo.toml | head -n1 | tr -d '"')
CP2CL_APP_ARCH = $(if $(filter aarch64,$(ARCH)),aarch64,armv7)
# Both boards run Linux, so the operating system half of the name is written
# here; the architecture half comes from the build.
CP2CL_APP = cp2cl-linux-$(CP2CL_APP_ARCH)
CP2CL_SITE = $(BR2_EXTERNAL_CP2CL_PATH)/../dist/app
CP2CL_SITE_METHOD = local
CP2CL_LICENSE = GPL-3.0-or-later
CP2CL_INSTALL_STAGING = NO

# The credentials the program reads at runtime, out of the directory it also
# writes its own state to. They are not in dist/ — that is the binaries and the
# images — and not in the binary either, so they come straight from the checkout,
# by the names the program asks for them: the MFi pair. The CarLife key is not
# one of them any more — the binary carries it, put there at build time from
# `CARLIFE_PRIVATE_KEY` — and its public half is a test fixture, written into the
# test itself rather than read from here.
CP2CL_CREDENTIALS = $(BR2_EXTERNAL_CP2CL_PATH)/../authentication
CP2CL_CREDENTIAL_FILES = \
	$(addprefix $(CP2CL_CREDENTIALS)/,identity.pk8 certificate.p7b)

define CP2CL_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/$(CP2CL_APP) $(TARGET_DIR)/usr/bin/cp2cl
	$(INSTALL) -d -m 0755 $(TARGET_DIR)/etc/cp2cl
	for f in $(CP2CL_CREDENTIAL_FILES); do \
		if [ -f "$$f" ]; then \
			$(INSTALL) -m 0600 "$$f" $(TARGET_DIR)/etc/cp2cl/; \
		fi; \
	done
endef

$(eval $(generic-package))
