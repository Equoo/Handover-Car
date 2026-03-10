cmake_minimum_required(VERSION 3.16)
project(streamer CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

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
