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

.PHONY: all test image

# What `make` on its own does: the binary the board runs, statically linked,
# at target/$(TARGET)/release/cp2cl.
all:
	$(RUN) cargo build --release --target $(TARGET)

test:
	$(RUN) cargo test

# Once per machine, and again only when `docker/Dockerfile` changes.
image:
	docker build -t $(IMAGE) docker/
