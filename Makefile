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
BOARD    ?= nanopi_duo2
UID_GID  := $(shell id -u):$(shell id -g)
# The uid of the build user in docker/buildroot.Dockerfile.
BR_UID   := 1000

# Both products land in dist/, and the card is named for what produced it: the
# same three facts the page shows as `v0.10.0 (9063c18-dirty)`, spelled as a file
# name has to — the tag on the commit, `dev` when it carries none, the short
# commit, and `-dirty` when the tree does not match it. `.img.gz` because that is
# what is handed out: what is written to a card is the `.img` inside, and the copy
# that travels is 15 MB of the 73.
#
# The Buildroot release is read back out of the Dockerfile that pins it rather
# than written here a second time: a second copy is a copy that goes stale.
DIST          := dist
BR_VERSION    := $(shell sed -n 's/^ARG BUILDROOT_VERSION=//p' docker/buildroot.Dockerfile)
CP2CL_VERSION := $(shell git describe --tags --exact-match 2>/dev/null || echo dev)
CP2CL_COMMIT  := $(shell git rev-parse --short HEAD 2>/dev/null || echo unknown)
DIRTY         := $(shell git status --porcelain 2>/dev/null)
FW := $(DIST)/buildroot-$(BR_VERSION)-cp2cl-$(CP2CL_VERSION)-$(CP2CL_COMMIT)$(if $(DIRTY),-dirty).img.gz

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

.PHONY: all app test image firmware buildroot-image

# Both products: the program the board runs, and the image it runs from.
# `make app` on its own is the program, which is what a change to the crate
# needs; the image is a tree, a toolchain and a kernel on every build.
all: app firmware

# The program, statically linked, staged into dist/ where buildroot/package/
# cp2cl points at it. Nothing else from the checkout is staged: a `local` site
# over the repository root would drag target/ through rsync on every build.
app:
	$(RUN) cargo build --release --target $(TARGET)
	mkdir -p $(DIST)
	install -m 0755 target/$(TARGET)/release/cp2cl $(DIST)/cp2cl

test:
	$(RUN) cargo test

# Once per machine, and again only when `docker/Dockerfile` changes.
image:
	docker build -t $(IMAGE) docker/

# BOARD=<name> builds any board that has a configs/<name>_defconfig.
#
# The build directory is a named volume, not a bind mount: the host's filesystem
# is case-insensitive and Buildroot's output is not. `dl/` goes in the volume too,
# with BR2_DL_DIR, because the default is beside the sources inside the image.
#
# The build runs as uid 1000, not as root: host-tar's configure refuses to run as
# root. The volume is root-owned when it is created, so the container starts as
# root, hands it over, and drops to the builder. Only the image crosses back, in
# a second container running as you — compressed on the way out, so what lands in
# dist/ is the file that is handed out rather than one more copy to make. `dd`
# rather than `cp` was how the raw card crossed back: on this bind mount cp
# finishes the write and then fails to deallocate the tail, which leaves a copy
# of the right length and an exit status that stops the build. `gzip -c` writes
# the bytes it compresses and nothing more, so there is no tail to give back.
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
firmware: app buildroot-image
	mkdir -p $(DIST)
	docker volume create $(BR_VOL) >/dev/null
	docker run --rm -u 0:0 -v $(BR_VOL):/build \
		-v "$(CURDIR)/buildroot":/ext:ro \
		-v "$(CURDIR)/$(DIST)":/dist:ro \
		-e BR2_DL_DIR=/build/dl \
		$(BR_IMAGE) sh -c 'chown $(BR_UID):$(BR_UID) /build \
			&& su -s /bin/sh builder -c "$(BR_FRESH) \
				&& make -C /opt/buildroot O=/build BR2_EXTERNAL=/ext $(BOARD)_defconfig \
				&& make -C /opt/buildroot O=/build -j$$(nproc) \
				&& make -C /opt/buildroot O=/build cp2cl-rebuild \
				&& make -C /opt/buildroot O=/build -j$$(nproc)"'
	docker run --rm -u $(UID_GID) -v $(BR_VOL):/build -v "$(CURDIR)/$(DIST)":/out \
		$(BR_IMAGE) sh -c 'gzip -9 -c /build/images/sdcard.img > /out/$(notdir $(FW))'
	@echo "image: $(FW)"

# Once per machine, and again only when the Dockerfile changes.
buildroot-image:
	docker build -t $(BR_IMAGE) -f docker/buildroot.Dockerfile docker/
