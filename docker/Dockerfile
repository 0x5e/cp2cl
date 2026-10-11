# Builds cp2cl for the NanoPi Duo2: armv7 hard-float, glibc.
#
# The compiler is a native arm64 binary that emits armv7 code, so this is an
# ordinary cross build — nothing is emulated. `--platform linux/arm64` comes
# from the build command rather than from here.
FROM rust:1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      gcc-arm-linux-gnueabihf \
      g++-arm-linux-gnueabihf \
      libc6-dev-armhf-cross \
      cmake \
      perl \
      pkg-config \
      git \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN rustup target add armv7-unknown-linux-gnueabihf

COPY armv7-toolchain.cmake /opt/armv7-toolchain.cmake

# Everything cargo needs to know about the cross compiler, so the run command
# carries no environment of its own.
ENV CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
    CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc \
    CXX_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-g++ \
    AR_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ar \
    RANLIB_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ranlib \
    STRIP_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-strip \
    CMAKE_TOOLCHAIN_FILE=/opt/armv7-toolchain.cmake

# A cargo home outside the image's own, world-writable so the build can run as
# the calling user and still have a cache to reuse.
ENV HOME=/tmp \
    CARGO_HOME=/cargo
RUN mkdir -p /cargo && chmod 1777 /cargo

WORKDIR /work
