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

# The credentials the program reads at runtime, out of the directory it also
# writes its own state to. They are not in dist/ — that is the binary and nothing
# else — and not in the binary either, so they come straight from the checkout,
# by the names the program asks for them: one of the three is the CarLife app's
# key, the other two are the MFi pair, and the public half beside them stays
# behind because only the test that checks the pair reads it.
CP2CL_CREDENTIALS = $(BR2_EXTERNAL_CP2CL_PATH)/../authentication
CP2CL_CREDENTIAL_FILES = \
	$(addprefix $(CP2CL_CREDENTIALS)/,identity.pk8 certificate.p7b carlife_md_rsa_private.pem)

define CP2CL_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/cp2cl $(TARGET_DIR)/usr/bin/cp2cl
	$(INSTALL) -d -m 0755 $(TARGET_DIR)/etc/cp2cl
	for f in $(CP2CL_CREDENTIAL_FILES); do \
		if [ -f "$$f" ]; then \
			$(INSTALL) -m 0600 "$$f" $(TARGET_DIR)/etc/cp2cl/; \
		fi; \
	done
endef

$(eval $(generic-package))
