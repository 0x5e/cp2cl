# Lets CMake cross-compile. `aws-lc-sys` arrives through idevice -> rustls and
# configures itself with CMake, which picks the host compiler unless it is handed
# a toolchain file.
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR arm)

set(MUSL_CROSS /opt/arm-linux-musleabihf-cross)

set(CMAKE_C_COMPILER   ${MUSL_CROSS}/bin/arm-linux-musleabihf-gcc)
set(CMAKE_CXX_COMPILER ${MUSL_CROSS}/bin/arm-linux-musleabihf-g++)
set(CMAKE_ASM_COMPILER ${MUSL_CROSS}/bin/arm-linux-musleabihf-gcc)
set(CMAKE_AR           ${MUSL_CROSS}/bin/arm-linux-musleabihf-ar)
set(CMAKE_RANLIB       ${MUSL_CROSS}/bin/arm-linux-musleabihf-ranlib)
set(CMAKE_STRIP        ${MUSL_CROSS}/bin/arm-linux-musleabihf-strip)

# No CMAKE_SYSROOT: the toolchain finds its own, and naming it twice is how a
# relocatable toolchain stops being one. This only has to stop CMake looking for
# libraries and headers on the host.
set(CMAKE_FIND_ROOT_PATH ${MUSL_CROSS}/arm-linux-musleabihf)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
