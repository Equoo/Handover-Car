cmake_minimum_required(VERSION 3.16)
project(streamer CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Without CMAKE_BUILD_TYPE, CMake passes no -O flag at all -- i.e. -O0. That is
# not a neutral default here: the capture callback memcpy's a full YUV420 frame
# (518 KB at 720x480) into the encoder queue on every frame, and on a Pi 4 an
# unoptimised build spends that time out of the frame period. Default to
# Release; an explicit -DCMAKE_BUILD_TYPE=<type> on the command line still wins.
if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING
        "Build type (Debug, Release, RelWithDebInfo, MinSizeRel)" FORCE)
    set_property(CACHE CMAKE_BUILD_TYPE PROPERTY STRINGS
        Debug Release RelWithDebInfo MinSizeRel)
endif()
message(STATUS "streamer build type: ${CMAKE_BUILD_TYPE}")

find_package(PkgConfig REQUIRED)

pkg_check_modules(GST      REQUIRED gstreamer-1.0)
pkg_check_modules(GST_APP  REQUIRED gstreamer-app-1.0)
pkg_check_modules(LIBCAM   REQUIRED libcamera)

add_executable(streamer src/streamer.cpp)

target_include_directories(streamer PRIVATE
    ${GST_INCLUDE_DIRS}
    ${GST_APP_INCLUDE_DIRS}
    ${LIBCAM_INCLUDE_DIRS}
)

target_link_libraries(streamer PRIVATE
    ${GST_LIBRARIES}
    ${GST_APP_LIBRARIES}
    ${LIBCAM_LIBRARIES}
    pthread
)

target_compile_options(streamer PRIVATE
    ${GST_CFLAGS_OTHER}
    ${LIBCAM_CFLAGS_OTHER}
)
