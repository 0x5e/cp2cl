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
# The uid of the build user in docker/buildroot.Dockerfile.
BR_UID   := 1000

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
# that causes arrive hours into a build. `dl/` is put in the volume as well, with
# BR2_DL_DIR, because Buildroot's default is `dl/` beside the sources — inside the
# image, where it is neither writable nor kept between runs.
#
# The build runs as the container's unprivileged user, not as root: Buildroot is
# not supported as root and host-tar's configure refuses to run there at all. The
# volume's root directory is root-owned when it is created and stays that way, so
# the container starts as root, hands it over, and drops to the builder with su —
# one container, because the chown only matters to the process that follows it.
# Only the finished image crosses back, in a second container running as you, so
# the file is not owned by uid 1000 on the host. `dd` rather than `cp`: on the
# bind mount into this machine cp finishes the write and then fails to deallocate
# the tail of the file it just made ("Invalid argument"), which leaves a copy of
# the right length and an exit status that stops the build.
firmware: dist buildroot-image
	mkdir -p $(BR_OUT)
	docker volume create $(BR_VOL) >/dev/null
	docker run --rm -u 0:0 -v $(BR_VOL):/build \
		-v "$(CURDIR)/buildroot":/ext:ro \
		-v "$(CURDIR)/dist":/dist:ro \
		-e BR2_DL_DIR=/build/dl \
		$(BR_IMAGE) sh -c 'chown $(BR_UID):$(BR_UID) /build \
			&& su -s /bin/sh builder -c "make -C /opt/buildroot O=/build BR2_EXTERNAL=/ext $(BOARD)_defconfig \
				&& make -C /opt/buildroot O=/build -j$$(nproc)"'
	docker run --rm -u $(UID_GID) -v $(BR_VOL):/build -v "$(CURDIR)/$(BR_OUT)":/out \
		$(BR_IMAGE) dd if=/build/images/sdcard.img of=/out/sdcard.img bs=1M status=none
	@echo "image: $(BR_OUT)/sdcard.img"

# Once per machine, and again only when the Dockerfile changes.
buildroot-image:
	docker build -t $(BR_IMAGE) -f docker/buildroot.Dockerfile docker/
