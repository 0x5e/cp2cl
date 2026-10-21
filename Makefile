# Building for the board, and the tests, in the container the repository ships.
#
# The container is what compiles this crate: the CarLife half talks to configfs,
# FunctionFS and nl80211, so there is nothing for a desktop to build. `-u` keeps
# the products owned by the caller, and `cp2cl-cargo` is the named volume holding
# the cargo cache, so only the first build downloads anything.
IMAGE  := cp2cl-armv7-builder
TARGET := armv7-unknown-linux-musleabihf
RUN    := docker run --rm -u "$$(id -u):$$(id -g)" \
            -v "$(CURDIR)":/work -v cp2cl-cargo:/cargo $(IMAGE)

# Buildroot is not in this repository; it is pinned as a submodule the way LIVI
# is, because a build system is a dependency and a checkout of one is 200 MB.
# What lives in buildroot/ is the external tree: the boards, the kernel
# fragments, and the one package, which is this crate.
BUILDROOT := vendor/buildroot
BR_OUT    := buildroot/output
BOARD     ?= nanopi_duo2

.PHONY: all test image dist firmware

# What `make` on its own does: the binary the board runs, statically linked,
# at target/$(TARGET)/release/cp2cl.
all:
	$(RUN) cargo build --release --target $(TARGET)

test:
	$(RUN) cargo test

# Once per machine, and again only when `docker/Dockerfile` changes.
image:
	docker build -t $(IMAGE) docker/

# The binary, staged where buildroot/package/cp2cl points at it. Nothing else
# from the checkout is copied: a `local` site over the repository root would
# drag target/ through rsync on every build.
dist: all
	mkdir -p dist
	install -m 0755 target/$(TARGET)/release/cp2cl dist/cp2cl

# BOARD=<name> builds any board that has a configs/<name>_defconfig.
firmware: dist
	$(MAKE) -C $(BUILDROOT) O=$(CURDIR)/$(BR_OUT) \
		BR2_EXTERNAL=$(CURDIR)/buildroot $(BOARD)_defconfig
	$(MAKE) -C $(BUILDROOT) O=$(CURDIR)/$(BR_OUT) \
		BR2_EXTERNAL=$(CURDIR)/buildroot
	@echo "image: $(BR_OUT)/images/sdcard.img"
