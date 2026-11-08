cmake_minimum_required(VERSION 3.16)
project(werewolf-window LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC OFF)

find_package(Qt6 REQUIRED COMPONENTS Widgets)

add_executable(werewolf-window main.cpp)
target_link_libraries(werewolf-window PRIVATE Qt6::Widgets)
