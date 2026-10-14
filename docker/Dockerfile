# Builds cp2cl for the NanoPi Duo2: armv7 hard-float, linked statically against
# musl. The binary carries its own C library, so which one the system it lands on
# uses — glibc or musl — is not a property of what we ship.
#
# The compiler is a native arm64 binary that emits armv7 code, so this is an
# ordinary cross build — nothing is emulated. `--platform linux/arm64` comes
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

# A cross toolchain that targets musl rather than glibc. musl.cc builds these and
# ships them as a tarball, relocatable, with the sysroot beside the compiler.
RUN curl -fsSL https://musl.cc/arm-linux-musleabihf-cross.tgz -o /tmp/musl-cross.tgz \
 && tar -xzf /tmp/musl-cross.tgz -C /opt \
 && rm /tmp/musl-cross.tgz
ENV PATH=/opt/arm-linux-musleabihf-cross/bin:$PATH

RUN rustup target add armv7-unknown-linux-musleabihf

COPY armv7-toolchain.cmake /opt/armv7-toolchain.cmake

# Everything cargo needs to know about the cross compiler, so the run command
# carries no environment of its own. That this target is linked statically is the
# repository's .cargo/config.toml, not this file.
ENV CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-musleabihf-gcc \
    CC_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-gcc \
    CXX_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-g++ \
    AR_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-ar \
    RANLIB_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-ranlib \
    STRIP_armv7_unknown_linux_musleabihf=arm-linux-musleabihf-strip \
    CMAKE_TOOLCHAIN_FILE=/opt/armv7-toolchain.cmake

# A cargo home outside the image's own, world-writable so the build can run as
# the calling user and still have a cache to reuse.
ENV HOME=/tmp \
    CARGO_HOME=/cargo
RUN mkdir -p /cargo && chmod 1777 /cargo

WORKDIR /work
