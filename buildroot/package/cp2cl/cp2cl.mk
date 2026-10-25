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
# `make app` stages the binary under dist/, and that directory is the source.
# Nothing else from the repository is copied, so `target/` never enters the
# rsync that a `local` site would otherwise run over the whole checkout.
#
################################################################################

# Read out of Cargo.toml rather than written here a second time: the copy that
# lived here said 0.9.1 while the crate said 0.11.0, and this number is what
# names the build directory and what the log prints on every package step.
CP2CL_VERSION = $(shell sed -n 's/^version = "//p' $(BR2_EXTERNAL_CP2CL_PATH)/../Cargo.toml | head -n1 | tr -d '"')
CP2CL_SITE = $(BR2_EXTERNAL_CP2CL_PATH)/../dist
CP2CL_SITE_METHOD = local
CP2CL_LICENSE = GPL-3.0-or-later
CP2CL_INSTALL_STAGING = NO

define CP2CL_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/cp2cl $(TARGET_DIR)/usr/bin/cp2cl
endef

$(eval $(generic-package))
