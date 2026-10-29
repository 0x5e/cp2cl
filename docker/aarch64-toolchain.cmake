# Lets CMake cross-compile for the UFI001C. `aws-lc-sys` arrives through
# idevice -> rustls and configures itself with CMake, which picks the host
# compiler unless it is handed a toolchain file.
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)

set(MUSL_CROSS /opt/aarch64-linux-musl-cross)

set(CMAKE_C_COMPILER   ${MUSL_CROSS}/bin/aarch64-linux-musl-gcc)
set(CMAKE_CXX_COMPILER ${MUSL_CROSS}/bin/aarch64-linux-musl-g++)
set(CMAKE_ASM_COMPILER ${MUSL_CROSS}/bin/aarch64-linux-musl-gcc)
set(CMAKE_AR           ${MUSL_CROSS}/bin/aarch64-linux-musl-ar)
set(CMAKE_RANLIB       ${MUSL_CROSS}/bin/aarch64-linux-musl-ranlib)
set(CMAKE_STRIP        ${MUSL_CROSS}/bin/aarch64-linux-musl-strip)

# No CMAKE_SYSROOT: the toolchain finds its own, and naming it twice is how a
# relocatable toolchain stops being one. This only has to stop CMake looking for
# libraries and headers on the host.
set(CMAKE_FIND_ROOT_PATH ${MUSL_CROSS}/aarch64-linux-musl)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
