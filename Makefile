# Building for the board, and the tests, in the container the repository ships.
#
# The container is what compiles this crate: the CarLife half talks to configfs,
# FunctionFS and nl80211, so there is nothing for a desktop to build. `-u` keeps
# the products owned by the caller, and `cp2cl-cargo` is the named volume holding
# the cargo cache, so only the first build downloads anything.
IMAGE  := cp2cl-builder

# Which image to build: `nanopi-duo2`, `orangepi-zero2w`, `msm8916-ufi001c`,
# or `all` for every board in turn. It decides the Buildroot defconfig a build
# starts from.
BOARDS := nanopi-duo2 orangepi-zero2w msm8916-ufi001c
BOARD  ?= all

# Which program to build: `aarch64`, `armv7`, or `all` for both in turn. A
# program is the architecture's, not the board's — the Zero 2W and the UFI001C
# run the same binary — so `make app` decides on this rather than on BOARD, and
# a board that was named sets it to its own instead of being asked twice.
ARCHS := aarch64 armv7
ARCH  ?= all

ifeq ($(filter $(BOARD),$(BOARDS) all),)
$(error BOARD is '$(BOARD)'; it is one of: $(BOARDS) all)
endif

ifeq ($(filter $(ARCH),$(ARCHS) all),)
$(error ARCH is '$(ARCH)'; it is one of: $(ARCHS) all)
endif

# The underscore spelling: Buildroot names its defconfigs with one, and the build
# volume is keyed the same way, so a board does not end up with two trees because
# its name was written the other way round. A volume that changes name is a tree
# built again from an empty download cache.
BOARD_CFG := $(subst -,_,$(BOARD))

# The architecture a board runs is written out per board, and not as "whatever
# is not the odd one out is armv7", which was a rule that held only while there
# were two boards.
BOARD_ARCH_nanopi-duo2     := armv7
BOARD_ARCH_orangepi-zero2w := aarch64
BOARD_ARCH_msm8916-ufi001c := aarch64

# A named board carries one architecture, so that is the one being built — the
# image of a board holding the other board's program is an image that does not
# run. So `make firmware BOARD=...` needs only the board, and naming both is
# allowed only when they agree.
ifneq ($(filter $(BOARD),$(BOARDS)),)
ifneq ($(ARCH),all)
ifneq ($(ARCH),$(BOARD_ARCH_$(BOARD)))
$(error ARCH is '$(ARCH)' but $(BOARD) is $(BOARD_ARCH_$(BOARD)))
endif
endif
ARCH := $(BOARD_ARCH_$(BOARD))
endif

# The target belongs to the architecture alone, so `make app ARCH=aarch64` and
# `make firmware BOARD=orangepi-zero2w` name the same compiler. Empty while the
# architecture is still `all`: only the rule that recurses on it runs then, and
# that rule hands every build one.
TARGET_aarch64   := aarch64-unknown-linux-musl
TARGET_armv7     := armv7-unknown-linux-musleabihf
CMAKE_TF_aarch64 := /opt/aarch64-toolchain.cmake
CMAKE_TF_armv7   := /opt/armv7-toolchain.cmake
TARGET   := $(TARGET_$(ARCH))
CMAKE_TF := $(CMAKE_TF_$(ARCH))

# The CarLife key is handed to the compiler rather than to the program: it is
# written into the binary at build time, out of the environment, so what is set
# in the calling shell is what the build carries. Unset, the build produces a
# binary without one, which is a supported way to run — the car is left
# unauthenticated and the streams run either way.
RUN    := docker run --rm -u "$$(id -u):$$(id -g)" \
            -e CMAKE_TOOLCHAIN_FILE=$(CMAKE_TF) -e CARLIFE_PRIVATE_KEY \
            -v "$(CURDIR)":/work -v cp2cl-cargo:/cargo $(IMAGE)

# Buildroot is not in this repository; it is a container the image is built in,
# because there is no Linux machine here and Buildroot does not run on macOS.
# What lives in buildroot/ is the external tree: the boards, the kernel
# fragments, and the one package, which is this crate. The checkout is mounted
# whole rather than that one directory, because the package reaches outside it:
# it installs the credentials from authentication/ into the image and reads the
# version out of Cargo.toml, and both are one level up from the external tree.
BR_IMAGE := cp2cl-buildroot
# One build volume per board. The boards cross-compile for two architectures,
# and Buildroot's output tree (host toolchain, per-package build directories,
# module tree) is architecture-specific: a shared volume makes the second
# board's build reuse the first board's objects, which fails somewhere deep in
# the toolchain rather than at the configuration step.
BR_VOL   := cp2cl-buildroot-out-$(BOARD_CFG)
UID_GID  := $(shell id -u):$(shell id -g)
# The uid of the build user in docker/buildroot.Dockerfile.
BR_UID   := 1000

# Both products land in dist/, one directory each — the program under app/, the
# image under firmware/ — and each is named for what produced it: the program for
# its target, the image for its board, because every board's image is kept side
# by side and no binary runs on another board.
#
# The image is named for the board and for this tree: the tag when the commit
# carries one, the short commit otherwise, and `-dirty` when the tree does not
# match it — the same facts the page shows as `v0.12.0 (8582d5a-dirty)`.
#
# Every board is handed out as one filesystem image, uncompressed, under the name
# the flashing tool is pointed at. The NanoPi's and the Zero 2W's are card images
# written over the whole card; the UFI001C's is the userdata partition, which is
# also where its kernel is — the partition table, lk2nd and the calibration data
# are left as they are.
#
# The Buildroot release is read back out of the Dockerfile that pins it rather
# than written here a second time: a second copy is a copy that goes stale. No
# product carries it in its name, but a change of release empties the volume.
DIST          := dist
BR_VERSION    := $(shell sed -n 's/^ARG BUILDROOT_VERSION=//p' docker/buildroot.Dockerfile)
# One identifier, not two: a tagged commit is named by its tag, and a commit
# without one by its short hash. The crate's version is not in the name — that
# number is written in Cargo.toml and means the release, not the commit.
CP2CL_NAME := $(shell git describe --tags --exact-match 2>/dev/null \
	|| git rev-parse --short HEAD 2>/dev/null || echo dev)
# The submodule carries our patches (see `patched`), so its worktree is always
# dirty by design; only a different commit is a change worth naming.
DIRTY         := $(shell git status --porcelain --ignore-submodules=dirty 2>/dev/null)

# `dist/app/cp2cl-<os>-<arch>`, out of the triple: `cp2cl-linux-armv7` for the
# NanoPi and `cp2cl-linux-aarch64` for the other two. Flat, one file per target:
# a copy taken from dist/ says which board it was built for in its own name.
APP_OS   := $(word 3,$(subst -, ,$(TARGET)))
APP_ARCH := $(word 1,$(subst -, ,$(TARGET)))
APP      := $(DIST)/app/cp2cl-$(APP_OS)-$(APP_ARCH)

# `dist/firmware/cp2cl-<tag-or-commit>[-dirty]-<board>.img`.
FW_DIR := $(DIST)/firmware
FW     := $(FW_DIR)/cp2cl-$(CP2CL_NAME)$(if $(DIRTY),-dirty)-$(BOARD).img

# What post-image.sh left in the volume's images/, which is what crosses back. The
# card boards write sdcard.img, because it is written over a card and has no other
# name; the UFI001C writes one partition, which is flashed on its own and is named
# for the board. Either way it lands in dist/ under the name above.
ifeq ($(BOARD),msm8916-ufi001c)
PACKED := cp2cl-$(BOARD).img
else
PACKED := sdcard.img
endif

# What a firmware build starts from.
#
# Buildroot decides what to rebuild from timestamps, and a checkout rewrites
# every one of them, so this decides from content instead — one fingerprint per
# product, in buildroot/cache.sh. One fingerprint over the whole tree charged
# every change the price of all of them: a change to the crate emptied the
# target tree and deleted the radio driver's build directory, which is a full
# recompile of the driver for a change that had nothing to do with it.
#
# The tree is what the card is, and Buildroot only ever adds to it — a file
# stays after the package that put it there leaves the configuration, and after
# the overlay stops shipping it. So the tree is emptied when the configuration
# or the board files change, and kept otherwise. `images/` is not touched: mkfs
# writes the root filesystem and genimage writes both partitions and the card,
# every run, out of a fixed list of files.
#
# Upstream has nothing that empties the tree and keeps the cache that produced
# it: `make clean` takes dl/ and every build directory with it, and the
# `clean-target` target proposed on the mailing list in 2015 was never merged.
# These are the two commands that are left, in that order — the tree goes, the
# install stamps go, and the next make repeats those installs out of the build
# directories, which is a copy and not a compile.
BR_CACHE := sh /src/buildroot/cache.sh

# A change of Buildroot release is not a change of configuration: host/ and
# build/ belong to the release that made them, and their stamps make the new one
# skip the installs it needs — a host python without its installer modules stops
# the build at the first python package. Nothing but dl/ survives a release
# change, and the stamp is the release rather than a file in board/, which only
# decides the tree.
BR_RELEASE := { [ -e /build/.br-$(BR_VERSION) ] || { \
	find /build -mindepth 1 -maxdepth 1 ! -name dl -exec rm -rf {} + \
	&& touch /build/.br-$(BR_VERSION); }; }

.PHONY: all app test image firmware buildroot-image patched clean

# `all` is not a board: it is every board, one build after another. Recursion
# rather than a rule that builds both in one pass, because the Rust target and
# the build volume are read once, when this makefile is, and one pass over two
# boards would carry the first board's values into the second.
#
# `app` is not in this list: a program belongs to an architecture, and two
# boards run the same one. It recurses on ARCH below instead, so `make app`
# builds each architecture once rather than each board's program.
ifeq ($(BOARD),all)
all test firmware:
	@for b in $(BOARDS); do \
		$(MAKE) --no-print-directory BOARD=$$b $@ || exit 1; \
	done
else

# Both products: the program the board runs, and the image it runs from.
# `make app` on its own is the program, which is what a change to the crate
# needs; the image is a tree, a toolchain and a kernel on every build.
all: app firmware

test: patched
	$(RUN) cargo test --workspace

# One board, out of the names at the top of this file; the defconfig is
# `configs/<board>_defconfig`, and BOARD=all builds each of them in turn.
#
# The build directory is a named volume, not a bind mount: the host's filesystem
# is case-insensitive and Buildroot's output is not. `dl/` goes in the volume too,
# with BR2_DL_DIR, because the default is beside the sources inside the image.
#
# The build runs as uid 1000, not as root: host-tar's configure refuses to run as
# root. The volume is root-owned when it is created, so the container starts as
# root, hands it over, and drops to the builder. Only the image crosses back, in
# a second container running as you.
#
# `cat` and not `cp` or `dd`: on this bind mount both of those finish the write
# and then fail to deallocate the tail, which leaves a copy of the right length
# and an exit status that stops the build. A redirect writes the bytes it is
# given and nothing more, so there is no tail to give back.
#
# `_defconfig` comes first because the fingerprints are taken from the `.config`
# it writes; `cache.sh config` then reads that against the previous one and
# takes the tree apart accordingly. `linux-configure` sits between the two cache
# passes because the module fingerprint is the kernel's final `.config`, and
# that only exists once the kernel has been configured. It is a no-op when the
# kernel already is.
#
# The recipe empties the tree before the make, when the configuration or the
# board files changed, so what the card carries is the configuration and the
# overlays and nothing else that was ever on the volume.
firmware: app buildroot-image
	mkdir -p $(FW_DIR)
	docker volume create $(BR_VOL) >/dev/null
	docker run --rm -u 0:0 -v $(BR_VOL):/build \
		-v "$(CURDIR)":/src:ro \
		-e BR2_DL_DIR=/build/dl \
		$(BR_IMAGE) sh -c 'chown $(BR_UID):$(BR_UID) /build \
			&& su -s /bin/sh builder -c "$(BR_RELEASE) \
				&& make -C /opt/buildroot O=/build BR2_EXTERNAL=/src/buildroot $(BOARD_CFG)_defconfig \
				&& FRESH=$(FRESH) $(BR_CACHE) config $(BOARD) $(APP_ARCH) \
				&& make -C /opt/buildroot O=/build BR2_EXTERNAL=/src/buildroot linux-configure \
				&& $(BR_CACHE) modules \
				&& make -C /opt/buildroot O=/build -j$$(nproc)"'
	docker run --rm -u $(UID_GID) -v $(BR_VOL):/build -v "$(CURDIR)/$(DIST)":/out \
		$(BR_IMAGE) sh -c 'cat /build/images/$(PACKED) > /out/firmware/$(notdir $(FW))'
	@echo "image: $(FW)"

endif

# The program, statically linked, staged as dist/app/cp2cl-<os>-<arch>, where
# buildroot/package/cp2cl reads it from. Nothing else from the checkout is
# staged: a `local` site over the repository root would drag target/ through
# rsync on every build.
#
# Two architectures are two targets and two products, and one make would carry
# the first one's values into the second — so `ARCH=all`, the default, is each
# of them in turn.
ifeq ($(ARCH),all)
app:
	@for a in $(ARCHS); do \
		$(MAKE) --no-print-directory ARCH=$$a app || exit 1; \
	done
else
app: patched
	$(RUN) cargo build --release --target $(TARGET)
	mkdir -p $(DIST)/app
	install -m 0755 target/$(TARGET)/release/cp2cl $(APP)
endif

# The submodule is the upstream tag, so whatever this tree needs that the tag
# does not carry arrives as a patch in vendor/patches/, applied into the
# submodule's worktree. `--check --reverse` first: a second `make` finds them
# already on and does nothing, and one that no longer applies stops the build
# instead of quietly building a tree without it.
#
# Outside the board branch, like `app` below: the patches are what the program
# compiles against, and a program is built for an architecture, not a board.
# Inside it, this rule did not exist for `make app` at all — and `.PHONY` above
# made that a dependency make called satisfied, so the compile went ahead on an
# unpatched tree.
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

# Both images a build runs in, and neither belongs to a board: they are built
# once per machine, and again only when `docker/` changes. Nothing depends on
# them, because a build that reached for `docker build` on every run would pay
# for it on every run. `cp2cl-builder` also needs musl.cc, which GitHub's
# runners cannot reach — there it is pulled from the registry instead, and a
# change to `docker/Dockerfile` has to be pushed again for them.
image:
	docker build -t $(IMAGE) docker/

buildroot-image:
	docker build -t $(BR_IMAGE) -f docker/buildroot.Dockerfile docker/

# `clean` takes back what the builds wrote and leaves the checkout as it was
# found: the products in dist/ and the cargo artifacts in target/.
#
# What it deliberately keeps is dl/. That is the download cache, the one part of
# a build volume a rebuild cannot make back, and it is worth an hour of fetching;
# everything beside it goes — the Buildroot tree, its stamps, the host tools and
# the images — because a build that was interrupted leaves stamps that say a
# package is finished when half of it is on disk, and no later make disagrees.
# The next build compiles; it does not fetch.
#
# Per board, or every board's when BOARD=all. This rule works from the board list
# rather than through BOARD_CFG, so it does not need the recursion the build
# targets use — nothing here is read once when this makefile is.
CLEAN_VOLS := $(foreach b,$(if $(filter all,$(BOARD)),$(BOARDS),$(BOARD)),\
	cp2cl-buildroot-out-$(subst -,_,$(b)))

clean:
	rm -rf $(DIST) target
	@for v in $(CLEAN_VOLS); do \
		docker volume inspect $$v >/dev/null 2>&1 || continue; \
		docker image inspect $(BR_IMAGE) >/dev/null 2>&1 || continue; \
		docker run --rm -u 0:0 -v $$v:/build $(BR_IMAGE) sh -c \
			'find /build -mindepth 1 -maxdepth 1 ! -name dl -exec rm -rf {} +' \
		&& printf 'emptied %s\n' $$v; \
	done
