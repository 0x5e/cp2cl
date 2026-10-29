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
# fragments, and the one package, which is this crate. The checkout is mounted
# whole rather than that one directory, because the package reaches outside it:
# it installs the credentials from authentication/ into the image and reads the
# version out of Cargo.toml, and both are one level up from the external tree.
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
# The submodule carries two patches (see `patched`), so its worktree is always
# dirty by design; only a different commit is a change worth naming.
DIRTY         := $(shell git status --porcelain --ignore-submodules=dirty 2>/dev/null)
FW := $(DIST)/buildroot-$(BR_VERSION)-cp2cl-$(CP2CL_VERSION)-$(CP2CL_COMMIT)$(if $(DIRTY),-dirty).img.gz

# What a firmware build starts from.
#
# The tree is what the card is, and Buildroot only ever adds to it — a file
# stays after the package that put it there leaves the configuration, and after
# the overlay stops shipping it. There is no way back to a tree that holds only
# what this run asked for. This volume was still carrying the whole module tree
# of kernel 6.6.160 beside the 6.18.58 one the card runs: 31 files, 350 KB, for
# a kernel nothing here runs.
#
# So the tree is emptied when what decides it changes, and kept otherwise. The
# Buildroot release, the generated `.config` and everything under `board/` —
# the overlays, the post-build script, the blobs it installs — are the whole of
# it, and that is what the tree is fingerprinted by. A build after a change to
# the crate then installs one package instead of thirty. Timestamps would empty
# the tree on every checkout: a checkout rewrites every mtime in the working
# tree and changes nothing else.
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
#
# `make firmware FRESH=1` empties the tree whether the fingerprint says so or
# not: the answer when the tree was edited in place while debugging.
BR_FINGERPRINT := { echo $(BR_VERSION); cat /build/.config; \
	find /src/buildroot/board -type f | LC_ALL=C sort | xargs -r cat; } \
	| md5sum | cut -c1-32 > /build/.target-fingerprint.now
BR_EMPTY := rm -rf /build/target && rm -f /build/build/*/.stamp_target_installed
ifeq ($(FRESH),1)
BR_FRESH := $(BR_EMPTY)
else
BR_FRESH := $(BR_FINGERPRINT) && { cmp -s /build/.target-fingerprint.now /build/.target-fingerprint \
	|| { $(BR_EMPTY) && mv /build/.target-fingerprint.now /build/.target-fingerprint; }; }
endif

# The one package whose source changes on every build. A `local` site is rsynced
# once, when `.stamp_rsynced` is written, and the tree is filled from the install
# stamps — so both go, and the rsync lands ahead of the install in the same run.
BR_REFRESH := rm -f /build/build/cp2cl-*/.stamp_rsynced /build/build/cp2cl-*/.stamp_target_installed

# A change of Buildroot release is not a change of configuration: host/ and
# build/ belong to the release that made them, and their stamps make the new one
# skip the installs it needs — a host python without its installer modules stops
# the build at the first python package. Nothing but dl/ survives a release
# change, and the stamp is the release rather than a file in board/, which only
# decides the tree.
BR_RELEASE := { [ -e /build/.br-$(BR_VERSION) ] || { \
	find /build -mindepth 1 -maxdepth 1 ! -name dl -exec rm -rf {} + \
	&& touch /build/.br-$(BR_VERSION); }; }

.PHONY: all app test image firmware buildroot-image patched

# Both products: the program the board runs, and the image it runs from.
# `make app` on its own is the program, which is what a change to the crate
# needs; the image is a tree, a toolchain and a kernel on every build.
all: app firmware

# The submodule is the upstream tag, so whatever this board needs that the tag
# does not carry arrives as a patch in vendor/patches/, applied into the
# submodule's worktree. `--check --reverse` first: a second `make` finds them
# already on and does nothing, and one that no longer applies stops the build
# instead of quietly building a tree without it.
PATCHES := $(sort $(wildcard vendor/patches/*.patch))

patched:
	@for p in $(PATCHES); do \
	  if git -C vendor/LIVI apply --check --reverse "$(CURDIR)/$$p" 2>/dev/null; then \
	    :; \
	  elif git -C vendor/LIVI apply "$(CURDIR)/$$p"; then \
	    echo "patched $$p"; \
	  else \
	    echo "$$p no longer applies to vendor/LIVI" >&2; \
	    exit 1; \
	  fi; \
	done

# The program, statically linked, staged into dist/ where buildroot/package/
# cp2cl points at it. Nothing else from the checkout is staged: a `local` site
# over the repository root would drag target/ through rsync on every build.
app: patched
	$(RUN) cargo build --release --target $(TARGET)
	mkdir -p $(DIST)
	install -m 0755 target/$(TARGET)/release/cp2cl $(DIST)/cp2cl

test: patched
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
# `BR_REFRESH` between the configuration and the make is what makes a new binary
# reach the card. `_defconfig` comes first because `BR_FRESH` fingerprints the
# `.config` it writes, and a build that fingerprints the previous one keeps a
# tree the configuration no longer describes.
#
# The recipe empties the tree before the make, when the configuration or the
# overlays changed, so what the card carries is the configuration and the
# overlays and nothing else that was ever on the volume.
firmware: app buildroot-image
	mkdir -p $(DIST)
	docker volume create $(BR_VOL) >/dev/null
	docker run --rm -u 0:0 -v $(BR_VOL):/build \
		-v "$(CURDIR)":/src:ro \
		-e BR2_DL_DIR=/build/dl \
		$(BR_IMAGE) sh -c 'chown $(BR_UID):$(BR_UID) /build \
			&& su -s /bin/sh builder -c "$(BR_RELEASE) \
				&& make -C /opt/buildroot O=/build BR2_EXTERNAL=/src/buildroot $(BOARD)_defconfig \
				&& $(BR_FRESH) \
				&& $(BR_REFRESH) \
				&& make -C /opt/buildroot O=/build -j$$(nproc)"'
	docker run --rm -u $(UID_GID) -v $(BR_VOL):/build -v "$(CURDIR)/$(DIST)":/out \
		$(BR_IMAGE) sh -c 'gzip -9 -c /build/images/sdcard.img > /out/$(notdir $(FW))'
	@echo "image: $(FW)"

# Once per machine, and again only when the Dockerfile changes.
buildroot-image:
	docker build -t $(BR_IMAGE) -f docker/buildroot.Dockerfile docker/
