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

# Buildroot is not in this repository; it is a container the image is built in,
# because there is no Linux machine here and Buildroot does not run on macOS.
# What lives in buildroot/ is the external tree: the boards, the kernel
# fragments, and the one package, which is this crate.
BR_IMAGE := cp2cl-buildroot
BR_VOL   := cp2cl-buildroot-out
BR_OUT   := buildroot/output
BOARD    ?= nanopi_duo2
UID_GID  := $(shell id -u):$(shell id -g)

.PHONY: all test image dist firmware buildroot-image

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
#
# The build directory is a named volume rather than a bind mount: the host's
# filesystem is case-insensitive and Buildroot's output is not, and the failures
# that causes arrive hours into a build. `dl/` lives in the volume too, so a
# second build downloads nothing.
#
# The build itself runs as root — a volume's root directory stays root-owned here
# whatever it is chowned to — and only the finished image crosses back, in a
# second container running as you so the file is not root-owned on the host.
firmware: dist buildroot-image
	mkdir -p $(BR_OUT)
	docker volume create $(BR_VOL) >/dev/null
	docker run --rm -v $(BR_VOL):/build \
		-v "$(CURDIR)/buildroot":/ext:ro \
		-v "$(CURDIR)/dist":/dist:ro \
		$(BR_IMAGE) sh -c 'make -C /opt/buildroot O=/build BR2_EXTERNAL=/ext $(BOARD)_defconfig && make -C /opt/buildroot O=/build'
	docker run --rm -u $(UID_GID) -v $(BR_VOL):/build -v "$(CURDIR)/$(BR_OUT)":/out \
		$(BR_IMAGE) cp /build/images/sdcard.img /out/
	@echo "image: $(BR_OUT)/sdcard.img"

# Once per machine, and again only when the Dockerfile changes.
buildroot-image:
	docker build -t $(BR_IMAGE) -f docker/buildroot.Dockerfile docker/
