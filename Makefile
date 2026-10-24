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

# What a firmware build starts from: no target tree, and no stamp claiming a
# package was installed into it. The tree is what the card is, and Buildroot
# only ever adds to it — a file stays after the package that put it there leaves
# the configuration, and after the overlay stops shipping it. There is no way
# back to a tree that holds only what this run asked for. This volume was still
# carrying the whole module tree of kernel 6.6.160 beside the 6.18.58 one the
# card runs: 31 files, 350 KB, for a kernel nothing here runs.
#
# Upstream has nothing that empties the tree and keeps the cache that produced
# it: `make clean` takes dl/ and every build directory with it, and the
# `clean-target` target proposed on the mailing list in 2015 was never merged.
# These are the two commands that are left, in that order — the tree goes, the
# install stamps go, and the next make repeats those installs out of the build
# directories, which is a copy and not a compile.
#
# `images/` is not touched: mkfs writes the root filesystem and genimage writes
# both partitions and the card, every run, out of a fixed list of files.
BR_FRESH := rm -rf /build/target && rm -f /build/build/*/.stamp_target_installed

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
# The build directory is a named volume, not a bind mount: the host's filesystem
# is case-insensitive and Buildroot's output is not. `dl/` goes in the volume too,
# with BR2_DL_DIR, because the default is beside the sources inside the image.
#
# The build runs as uid 1000, not as root: host-tar's configure refuses to run as
# root. The volume is root-owned when it is created, so the container starts as
# root, hands it over, and drops to the builder. Only the image crosses back, in
# a second container running as you. `dd` rather than `cp`: on this bind mount cp
# finishes the write and then fails to deallocate the tail, which leaves a copy of
# the right length and an exit status that stops the build.
#
# `cp2cl-rebuild` between the two makes is what makes a new binary reach the card.
# A `local` site is rsynced once, when `.stamp_rsynced` is written, so a second
# `make firmware` would find every stamp in place and produce an image with the
# previous binary on it. The rebuild has to come after the first make: on a fresh
# volume there is no target directory to install into.
#
# The recipe opens with `BR_FRESH`: the target tree is emptied on every build, so
# what the card carries is the configuration and the overlays and nothing else
# that was ever on the volume.
firmware: dist buildroot-image
	mkdir -p $(BR_OUT)
	docker volume create $(BR_VOL) >/dev/null
	docker run --rm -u 0:0 -v $(BR_VOL):/build \
		-v "$(CURDIR)/buildroot":/ext:ro \
		-v "$(CURDIR)/dist":/dist:ro \
		-e BR2_DL_DIR=/build/dl \
		$(BR_IMAGE) sh -c 'chown $(BR_UID):$(BR_UID) /build \
			&& su -s /bin/sh builder -c "$(BR_FRESH) \
				&& make -C /opt/buildroot O=/build BR2_EXTERNAL=/ext $(BOARD)_defconfig \
				&& make -C /opt/buildroot O=/build -j$$(nproc) \
				&& make -C /opt/buildroot O=/build cp2cl-rebuild \
				&& make -C /opt/buildroot O=/build -j$$(nproc)"'
	docker run --rm -u $(UID_GID) -v $(BR_VOL):/build -v "$(CURDIR)/$(BR_OUT)":/out \
		$(BR_IMAGE) dd if=/build/images/sdcard.img of=/out/sdcard.img bs=1M status=none
	@echo "image: $(BR_OUT)/sdcard.img"

# Once per machine, and again only when the Dockerfile changes.
buildroot-image:
	docker build -t $(BR_IMAGE) -f docker/buildroot.Dockerfile docker/
