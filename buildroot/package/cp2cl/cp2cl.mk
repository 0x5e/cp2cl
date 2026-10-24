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

CP2CL_VERSION = 0.9.1
CP2CL_SITE = $(BR2_EXTERNAL_CP2CL_PATH)/../dist
CP2CL_SITE_METHOD = local
CP2CL_LICENSE = GPL-3.0-or-later
CP2CL_INSTALL_STAGING = NO

define CP2CL_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/cp2cl $(TARGET_DIR)/usr/bin/cp2cl
endef

$(eval $(generic-package))
