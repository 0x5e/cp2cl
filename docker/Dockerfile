# Builds cp2cl for the two boards this repository ships: the NanoPi Duo2 (armv7,
# hard-float) and the UFI001C (aarch64). Both link statically against musl, so
# which C library the system they land on uses is not a property of what we ship.
#
# The compilers are native arm64 binaries that emit the target code, so this is
# an ordinary cross build — nothing is emulated. `--platform linux/arm64` comes
# from the run command rather than from here.
FROM rust:1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      curl \
      xz-utils \
      ca-certificates \
      cmake \
      perl \
      pkg-config \
      git \
 && rm -rf /var/lib/apt/lists/*

# Cross toolchains that target musl rather than glibc. musl.cc builds these and
# ships them as tarballs, relocatable, with the sysroot beside the compiler.
# Both are installed because the same image builds both boards; which one a build
# uses is chosen by the Makefile, not by this file.
RUN curl -fsSL https://musl.cc/arm-linux-musleabihf-cross.tgz -o /tmp/arm.tgz \
 && curl -fsSL https://musl.cc/aarch64-linux-musl-cross.tgz -o /tmp/aarch64.tgz \
 && tar -xzf /tmp/arm.tgz -C /opt \
 && tar -xzf /tmp/aarch64.tgz -C /opt \
 && rm /tmp/arm.tgz /tmp/aarch64.tgz
ENV PATH=/opt/arm-linux-musleabihf-cross/bin:/opt/aarch64-linux-musl-cross/bin:$PATH

RUN rustup target add armv7-unknown-linux-musleabihf aarch64-unknown-linux-musl

COPY armv7-toolchain.cmake /opt/armv7-toolchain.cmake
COPY aarch64-toolchain.cmake /opt/aarch64-toolchain.cmake

# Everything cargo needs to know about the cross compilers, so the run command
# carries no environment of its own. That these targets are linked statically is
# the repository's .cargo/config.toml, not this file.
#
# CMAKE_TOOLCHAIN_FILE is the armv7 one: a `docker run` of this image with no
# override is the NanoPi build, which is what it was before the second board
# existed. The Makefile passes the aarch64 file for BOARD=ufi001c.
ENV CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-musleabihf-gcc \
    CC_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-gcc \
    CXX_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-g++ \
    AR_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-ar \
    RANLIB_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-ranlib \
    STRIP_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-strip \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
    CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc \
    CXX_aarch64_unknown_linux_musl=aarch64-linux-musl-g++ \
    AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar \
    RANLIB_aarch64_unknown_linux_musl=aarch64-linux-musl-ranlib \
    STRIP_aarch64_unknown_linux_musl=aarch64-linux-musl-strip \
    CMAKE_TOOLCHAIN_FILE=/opt/armv7-toolchain.cmake

# A cargo home outside the image's own, world-writable so the build can run as
# the calling user and still have a cache to reuse.
ENV HOME=/tmp \
    CARGO_HOME=/cargo
RUN mkdir -p /cargo && chmod 1777 /cargo

WORKDIR /work
