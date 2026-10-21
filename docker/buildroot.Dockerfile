# Buildroot's host. There is no Linux machine here and Buildroot does not run on
# macOS, so the container is the only host it has.
#
# The version is pinned rather than latest: the image a board runs should be
# reproducible from a checkout, and "whatever the mirror had that day" is not.
FROM debian:bookworm-slim

ARG BUILDROOT_VERSION=2026.02.3

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        bc bzip2 ca-certificates cpio curl file g++ gcc git libncurses-dev \
        make patch perl python3 rsync sed tar unzip wget which \
 && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 --branch "${BUILDROOT_VERSION}" \
        https://github.com/buildroot/buildroot /opt/buildroot

# The build directory is a volume, not a bind mount — see the Makefile.
WORKDIR /build
