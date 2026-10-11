# Lets CMake cross-compile. `aws-lc-sys` arrives through idevice -> rustls and
# configures itself with CMake, which picks the host compiler unless it is
# handed a toolchain file.
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR arm)

set(CMAKE_C_COMPILER   arm-linux-gnueabihf-gcc)
set(CMAKE_CXX_COMPILER arm-linux-gnueabihf-g++)
set(CMAKE_ASM_COMPILER arm-linux-gnueabihf-gcc)
set(CMAKE_AR           arm-linux-gnueabihf-ar)
set(CMAKE_RANLIB       arm-linux-gnueabihf-ranlib)
set(CMAKE_STRIP        arm-linux-gnueabihf-strip)

set(CMAKE_FIND_ROOT_PATH /usr/arm-linux-gnueabihf)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
