# Buildroot's host. There is no Linux machine here and Buildroot does not run on
# macOS, so the container is the only host it has.
#
# The version is pinned rather than latest: the image a board runs should be
# reproducible from a checkout, and "whatever the mirror had that day" is not.
FROM debian:bookworm-slim

ARG BUILDROOT_VERSION=2026.02.3

# dosfstools and mtools are genimage's, not Buildroot's: the boot partition is a
# vfat image, and genimage shells out to mkdosfs and mcopy to make one.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        bc bzip2 ca-certificates cpio curl dosfstools file g++ gcc git \
        libncurses-dev make mtools patch perl python3 rsync sed tar unzip wget \
        which \
 && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 --branch "${BUILDROOT_VERSION}" \
        https://github.com/buildroot/buildroot /opt/buildroot

# Not root. Buildroot is not supported as root and says so: host-tar's configure
# refuses outright, and FORCE_UNSAFE_CONFIGURE=1 is a flag whose name is the
# warning. A fixed uid, because the build directory is a volume whose root has to
# be chowned to it once — see the Makefile.
RUN useradd --create-home --uid 1000 builder

# The build directory is a volume, not a bind mount — see the Makefile.
WORKDIR /build

USER builder
